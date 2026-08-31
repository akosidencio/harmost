//! The operator surface: liveness, readiness and status.
//!
//! # Why this is a separate listener
//!
//! Not a path on the traffic listener, for two reasons. `/status` is a
//! perfectly ordinary application route, so a path would collide with the
//! origin's own URL space and the collision would be silent. And it would
//! publish backend health, cache occupancy and the configuration generation to
//! anyone who can reach the site — a reconnaissance gift, and on a governor
//! whose whole job is to be the thing that stays up during an incident.
//!
//! Bind it to loopback or a private address. Harmost refuses to start if it is
//! bound to the same address as the traffic listener; it cannot tell whether
//! `0.0.0.0` is safe on your network, and says so in `harmost check` instead.
//!
//! # No client-controlled cardinality
//!
//! Nothing here is parameterised. There is no path segment, query key, header
//! or body that changes what is computed or how much of it there is. Every
//! response is built from a fixed set of fields whose sizes are bounded by the
//! configuration file: route ids, upstream addresses, limiter names. This is
//! the same rule the Prometheus labels follow, for the same reason — an
//! operator surface must never be a way to make the process do unbounded work,
//! least of all during the incident it exists to explain.
//!
//! # Readiness is a claim about *this instance*
//!
//! `/health/ready` answers 503 while draining, so a load balancer stops
//! sending new work before the process starts shutting down. That gap is the
//! whole reason zero-downtime restarts work, and it is why draining is a state
//! Harmost enters some seconds *before* it begins to exit.

pub mod drain;

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use http::{Response, StatusCode};
use pingora_core::apps::http_app::ServeHttp;
use pingora_core::protocols::http::ServerSession;

use crate::admission::AdmissionController;
use crate::admission::limiter::Limiter;
use crate::cache::BoundedStore;
use crate::policy::PolicySnapshot;
use crate::proxy::spool::SpoolBudget;
use crate::telemetry::json::{field_str, quoted};
use crate::upstream::UpstreamPool;
use crate::upstream::breaker::BreakerState;
use crate::upstream::retry::RetryBudget;
use drain::DrainState;

/// Everything the admin endpoints read. Every field is a handle to live state,
/// never a copy — a status document assembled from a snapshot taken at startup
/// is worse than none at all.
pub struct Admin {
    pub started: Instant,
    pub config_path: String,
    pub policy: Arc<ArcSwap<PolicySnapshot>>,
    pub admission: Arc<AdmissionController>,
    pub upstreams: Arc<UpstreamPool>,
    pub store: &'static BoundedStore,
    pub spool: Arc<SpoolBudget>,
    pub upgrades: Arc<Limiter>,
    pub retry: Arc<RetryBudget>,
    pub drain: Arc<DrainState>,
    /// Report not-ready when no upstream is passing its health check. Off by
    /// default; see [`crate::config::schema::Admin`].
    pub require_healthy_upstream: bool,
}

/// Why readiness said no. A closed set, so it is safe to put in a response and
/// safe to alert on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotReady {
    Draining,
    NoHealthyUpstream,
}

impl NotReady {
    pub fn as_str(self) -> &'static str {
        match self {
            NotReady::Draining => "draining",
            NotReady::NoHealthyUpstream => "no_healthy_upstream",
        }
    }
}

impl Admin {
    /// The readiness decision, separated from its HTTP rendering so it can be
    /// tested without a socket.
    pub fn readiness(&self) -> Result<(), NotReady> {
        if self.drain.is_draining() {
            return Err(NotReady::Draining);
        }
        if self.require_healthy_upstream && self.upstreams.healthy_count() == 0 {
            return Err(NotReady::NoHealthyUpstream);
        }
        Ok(())
    }

    fn liveness_body(&self) -> String {
        let mut s = String::from("{");
        field_str(&mut s, "status", "live");
        field_str(&mut s, "version", env!("CARGO_PKG_VERSION"));
        let _ = write!(
            s,
            "\"uptime_seconds\":{}}}",
            self.started.elapsed().as_secs()
        );
        s
    }

    fn readiness_body(&self) -> (StatusCode, String) {
        let verdict = self.readiness();
        let mut s = String::from("{");
        let _ = write!(s, "\"ready\":{},", verdict.is_ok());
        field_str(
            &mut s,
            "reason",
            verdict.err().map_or("ok", NotReady::as_str),
        );
        let _ = write!(
            s,
            "\"draining\":{},\"healthy_upstreams\":{},\"upstreams\":{}}}",
            self.drain.is_draining(),
            self.upstreams.healthy_count(),
            self.upstreams.len()
        );
        let status = if verdict.is_ok() {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        };
        (status, s)
    }

    /// The full status document.
    ///
    /// Everything in it is either a constant, a number, or a string that came
    /// from the configuration file. Nothing a client sent reaches it.
    pub fn status_body(&self) -> String {
        let policy = self.policy.load();
        let cfg = &policy.config;
        let mut s = String::with_capacity(1024);
        s.push('{');
        field_str(&mut s, "version", env!("CARGO_PKG_VERSION"));
        let _ = write!(
            s,
            "\"uptime_seconds\":{},",
            self.started.elapsed().as_secs()
        );

        // ---- configuration
        s.push_str("\"config\":{");
        field_str(&mut s, "path", &self.config_path);
        let _ = write!(
            s,
            "\"schema_version\":{},\"generation\":{},\"fingerprint\":{},\"routes\":{},\"features\":[",
            crate::config::SCHEMA_VERSION,
            policy.generation,
            policy.fingerprint,
            cfg.routes.len()
        );
        let mut first = true;
        for feature in compiled_features() {
            if !first {
                s.push(',');
            }
            quoted(&mut s, feature);
            first = false;
        }
        s.push_str("]},");

        // ---- drain
        s.push_str("\"drain\":{");
        let _ = write!(s, "\"draining\":{},", self.drain.is_draining());
        match self.drain.draining_for() {
            Some(d) => {
                let _ = write!(s, "\"draining_for_seconds\":{},", d.as_secs());
            }
            None => s.push_str("\"draining_for_seconds\":null,"),
        }
        field_str(&mut s, "reason", self.drain.reason());
        s.truncate(s.trim_end_matches(',').len());
        s.push_str("},");

        // ---- admission
        s.push_str("\"admission\":{\"global\":");
        limiter_json(&mut s, self.admission.global());
        s.push_str(",\"tiers\":[");
        for (i, limiter) in self.admission.tier_limiters().iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            limiter_json(&mut s, limiter);
        }
        s.push_str("],\"routes\":[");
        for (i, limiter) in self.admission.route_limiters().iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            limiter_json(&mut s, limiter);
        }
        s.push_str("]},");

        // ---- upstreams
        //
        // `healthy` and `ejected` are separate answers to separate questions,
        // and a backend can be both healthy and ejected at once: that is what
        // an origin answering its probe while failing its renders looks like,
        // and collapsing the two into one field would hide the distinction
        // that makes passive observation worth having.
        s.push_str("\"upstreams\":[");
        for (i, backend) in self.upstreams.backends().iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            let (ok, failed, trips) = self.upstreams.breaker_counts(backend.id);
            s.push('{');
            field_str(&mut s, "address", &backend.address);
            let _ = write!(
                s,
                "\"healthy\":{},\"ejected\":{},\"in_flight\":{},\"latency_ewma_us\":{},\
                 \"window\":{{\"ok\":{ok},\"failed\":{failed}}},\"trips\":{trips}}}",
                self.upstreams.is_healthy(backend.id),
                self.upstreams.breaker_state(backend.id) == BreakerState::Open,
                self.upstreams.in_flight(backend.id),
                self.upstreams.ewma_micros(backend.id),
            );
        }
        let _ = write!(
            s,
            "],\"cache\":{{\"enabled\":{},\"bytes_used\":{},\"max_bytes\":{},\"entries\":{}}},",
            cfg.cache.enabled,
            self.store.bytes_used(),
            self.store.limit(),
            self.store.entries()
        );

        let _ = write!(
            s,
            "\"spool\":{{\"bytes_used\":{},\"max_bytes\":{}}},",
            self.spool.used(),
            self.spool.limit()
        );
        {
            let now = self.upstreams.now_ms();
            let (attempts, retries) = self.retry.counts(now);
            let _ = write!(
                s,
                "\"retry\":{{\"enabled\":{},\"max_attempts\":{},\"window_attempts\":{attempts},\
                 \"window_retries\":{retries},\"budget\":{}}},",
                self.retry.enabled(),
                self.retry.max_attempts(),
                self.retry.allowance(now),
            );
        }
        let _ = write!(
            s,
            "\"upgrades\":{{\"enabled\":{},\"limit\":{},\"in_flight\":{}}}}}",
            cfg.upgrade.enabled,
            self.upgrades.limit(),
            self.upgrades
                .limit()
                .saturating_sub(self.upgrades.available())
        );
        s
    }
}

/// Which optional features this binary was compiled with.
///
/// In the status document because "is TLS available in this build" is
/// otherwise only answerable by trying it, and the answer differs between a
/// container image and a local `cargo build`.
fn compiled_features() -> Vec<&'static str> {
    let mut features = Vec::new();
    if cfg!(feature = "tls") {
        features.push("tls");
    }
    features
}

fn limiter_json(s: &mut String, limiter: &Limiter) {
    s.push('{');
    field_str(s, "name", limiter.name());
    let limit = limiter.limit();
    let _ = write!(
        s,
        "\"limit\":{},\"in_flight\":{},\"available\":{},\"queue_depth\":{},\
         \"queue_max\":{},\"queue_timeout_ms\":{}}}",
        limit,
        limit.saturating_sub(limiter.available()),
        limiter.available(),
        limiter.queue_depth(),
        limiter.queue_max(),
        limiter.queue_timeout().as_millis()
    );
}

const INDEX: &str = concat!(
    "harmost ",
    env!("CARGO_PKG_VERSION"),
    " admin\n\n",
    "GET /health/live   always 200 while the process is running\n",
    "GET /health/ready  503 while draining or, if configured, while no upstream is healthy\n",
    "GET /status        configuration generation, backend state, cache and spool usage\n",
);

#[async_trait]
impl ServeHttp for Admin {
    async fn response(&self, session: &mut ServerSession) -> Response<Vec<u8>> {
        let req = session.req_header();
        // A `HEAD` is answered like a `GET` minus the body, which Pingora does
        // by way of the response it is handed; anything else is refused
        // outright. This surface has no state to change.
        if req.method != http::Method::GET && req.method != http::Method::HEAD {
            return text(
                StatusCode::METHOD_NOT_ALLOWED,
                "only GET and HEAD are served here\n",
            );
        }
        // Path only. A query string cannot change anything below, and is
        // ignored rather than rejected so that a `?v=1` cache-buster from a
        // dashboard does not read as an outage.
        match req.uri.path() {
            "/health/live" | "/healthz" => json(StatusCode::OK, self.liveness_body()),
            "/health/ready" | "/readyz" => {
                let (status, body) = self.readiness_body();
                json(status, body)
            }
            "/status" => json(StatusCode::OK, self.status_body()),
            "/" => text(StatusCode::OK, INDEX),
            _ => text(StatusCode::NOT_FOUND, "not found\n"),
        }
    }
}

fn json(status: StatusCode, body: String) -> Response<Vec<u8>> {
    build(status, "application/json", body.into_bytes())
}

fn text(status: StatusCode, body: &str) -> Response<Vec<u8>> {
    build(
        status,
        "text/plain; charset=utf-8",
        body.as_bytes().to_vec(),
    )
}

/// `no-store` on every answer.
///
/// A cached readiness response is a load balancer routing to an instance that
/// stopped being ready some seconds ago, which is the exact failure this
/// endpoint exists to prevent.
fn build(status: StatusCode, content_type: &str, body: Vec<u8>) -> Response<Vec<u8>> {
    Response::builder()
        .status(status)
        .header(http::header::CONTENT_TYPE, content_type)
        .header(http::header::CACHE_CONTROL, "no-store")
        .header(http::header::CONTENT_LENGTH, body.len())
        .body(body)
        // The builder fails only on an invalid status or header, all of which
        // are constants here. A 500 with no body is still a truthful answer,
        // and it keeps the admin surface from being the one thing that can
        // panic a proxy.
        .unwrap_or_else(|_| {
            let mut fallback = Response::new(Vec::new());
            *fallback.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            fallback
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::LoadBalancing;
    use std::time::Duration;

    fn admin(require_healthy: bool) -> Admin {
        let yaml = "version: 1\norigin:\n  upstreams: [\"127.0.0.1:3000\", \"127.0.0.2:3000\"]\n\
                    routes:\n  - id: products\n    match: \"/products/**\"\n    concurrency:\n      max: 4\n";
        let cfg: crate::config::Config = serde_saphyr::from_str(yaml).unwrap();
        crate::config::validation::validate(&cfg).unwrap();
        let policy = PolicySnapshot::build(cfg, 7).unwrap();
        let admission = Arc::new(AdmissionController::new(
            10,
            5,
            Duration::from_millis(250),
            &crate::config::schema::Priorities::default(),
        ));
        admission.route_limiter("products", 4, 2, Duration::from_millis(100));
        Admin {
            started: Instant::now(),
            config_path: "/etc/harmost/harmost.yaml".to_string(),
            policy: Arc::new(ArcSwap::from(policy)),
            admission,
            upstreams: Arc::new({
                let pool = UpstreamPool::new(
                    &["127.0.0.1:3000".to_string(), "127.0.0.2:3000".to_string()],
                    LoadBalancing::RoundRobin,
                    &crate::config::schema::Breaker::default(),
                )
                .unwrap();
                pool.assume_healthy();
                pool
            }),
            store: BoundedStore::new(64 * 1024 * 1024),
            spool: SpoolBudget::new(1024 * 1024),
            upgrades: Limiter::new("upgrade", 100, 0, Duration::ZERO),
            retry: Arc::new(RetryBudget::new(&crate::config::schema::Retry::default())),
            drain: Arc::new(DrainState::new()),
            require_healthy_upstream: require_healthy,
        }
    }

    #[test]
    fn a_fresh_instance_is_ready() {
        assert_eq!(admin(false).readiness(), Ok(()));
    }

    #[test]
    fn draining_makes_readiness_fail_before_anything_stops_serving() {
        // The whole point: the balancer has to learn we are going away while
        // we are still able to answer.
        let a = admin(false);
        a.drain.begin("test");
        assert_eq!(a.readiness(), Err(NotReady::Draining));
        let (status, body) = a.readiness_body();
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains(r#""ready":false"#), "{body}");
        assert!(body.contains(r#""reason":"draining""#), "{body}");
    }

    #[test]
    fn liveness_does_not_follow_readiness() {
        // A draining instance is still alive. Conflating the two makes an
        // orchestrator kill the process mid-drain, which is the opposite of
        // what draining is for.
        let a = admin(false);
        a.drain.begin("test");
        assert!(a.liveness_body().contains(r#""status":"live""#));
    }

    #[test]
    fn an_unhealthy_pool_only_fails_readiness_when_asked_for() {
        let lenient = admin(false);
        let strict = admin(true);
        for pool in [&lenient.upstreams, &strict.upstreams] {
            pool.set_healthy(0, false);
            pool.set_healthy(1, false);
        }
        // The default is deliberate: Harmost still serves a fully unhealthy
        // pool, so taking every replica out of rotation would turn a degraded
        // origin into a total outage at the edge too.
        assert_eq!(lenient.readiness(), Ok(()));
        assert_eq!(strict.readiness(), Err(NotReady::NoHealthyUpstream));

        strict.upstreams.set_healthy(1, true);
        assert_eq!(strict.readiness(), Ok(()));
    }

    #[test]
    fn the_status_document_is_parseable_json_with_the_operational_fields() {
        let a = admin(false);
        let body = a.status_body();
        assert!(body.starts_with('{') && body.ends_with('}'), "{body}");
        assert_balanced(&body);
        for expected in [
            r#""schema_version":1"#,
            r#""generation":7"#,
            r#""fingerprint":"#,
            r#""routes":1"#,
            r#""draining":false"#,
            r#""path":"/etc/harmost/harmost.yaml""#,
            r#""address":"127.0.0.1:3000""#,
            r#""healthy":true"#,
            r#""name":"global""#,
            r#""name":"products""#,
            r#""limit":10"#,
            r#""queue_timeout_ms":250"#,
            r#""bytes_used":0"#,
            r#""max_bytes":67108864"#,
            // Resilience state: the three priority tiers, per-backend breaker
            // and load figures, and the retry budget.
            r#""name":"tier:high""#,
            r#""name":"tier:normal""#,
            r#""name":"tier:low""#,
            r#""ejected":false"#,
            r#""in_flight":0"#,
            r#""latency_ewma_us":0"#,
            r#""window":{"ok":0,"failed":0}"#,
            r#""trips":0"#,
            r#""retry":{"enabled":false"#,
        ] {
            assert!(body.contains(expected), "missing {expected} in {body}");
        }
    }

    /// A backend that passes its probe and fails its renders is the case
    /// passive observation exists for, so the two states have to be separately
    /// visible rather than collapsed into one field.
    #[test]
    fn the_status_document_reports_health_and_ejection_separately() {
        let breaker = crate::config::schema::Breaker {
            enabled: true,
            min_requests: 2,
            failure_percent: 50,
            max_ejected_percent: 50,
            ..Default::default()
        };
        let pool = UpstreamPool::new(
            &["127.0.0.1:3000".to_string(), "127.0.0.2:3000".to_string()],
            LoadBalancing::RoundRobin,
            &breaker,
        )
        .unwrap();
        pool.assume_healthy();
        pool.record_outcome(0, None, false);
        pool.record_outcome(0, None, false);

        let mut a = admin(false);
        a.upstreams = Arc::new(pool);
        let body = a.status_body();
        assert_balanced(&body);
        assert!(
            body.contains(r#""address":"127.0.0.1:3000","healthy":true,"ejected":true"#),
            "{body}"
        );
        assert!(
            body.contains(r#""address":"127.0.0.2:3000","healthy":true,"ejected":false"#),
            "{body}"
        );
        assert!(body.contains(r#""trips":1"#), "{body}");
    }

    #[test]
    fn backend_state_in_the_status_document_tracks_health_checking() {
        let a = admin(false);
        a.upstreams.set_healthy(0, false);
        let body = a.status_body();
        // Two backends, one of each state — a document that always said
        // `true` would be worse than not publishing the field.
        assert_eq!(body.matches(r#""healthy":false"#).count(), 1, "{body}");
        assert_eq!(body.matches(r#""healthy":true"#).count(), 1, "{body}");
    }

    #[test]
    fn the_status_document_reports_the_live_generation_not_the_startup_one() {
        let a = admin(false);
        let cfg = a.policy.load().config.clone();
        a.policy.store(PolicySnapshot::build(cfg, 8).unwrap());
        assert!(a.status_body().contains(r#""generation":8"#));
    }

    #[test]
    fn the_drain_reason_is_a_closed_set_not_free_text() {
        // It is rendered into JSON, so it must not be able to carry anything
        // a caller chose. `begin` takes a &'static str for that reason.
        let a = admin(false);
        a.drain.begin("sigterm");
        assert!(a.status_body().contains(r#""reason":"sigterm""#));
    }

    /// Braces and quotes balance, which is the cheap stand-in for a parser we
    /// deliberately do not depend on.
    fn assert_balanced(body: &str) {
        let mut depth = 0i32;
        let mut in_string = false;
        let mut escaped = false;
        for c in body.chars() {
            if in_string {
                match c {
                    _ if escaped => escaped = false,
                    '\\' => escaped = true,
                    '"' => in_string = false,
                    _ => {}
                }
                continue;
            }
            match c {
                '"' => in_string = true,
                '{' | '[' => depth += 1,
                '}' | ']' => depth -= 1,
                _ => {}
            }
            assert!(depth >= 0, "unbalanced close in {body}");
        }
        assert!(!in_string, "unterminated string in {body}");
        assert_eq!(depth, 0, "unbalanced braces in {body}");
    }
}
