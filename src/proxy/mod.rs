//! The Pingora proxy layer.
//!
//! Order of operations is the whole design, and it is why admission lives in
//! [`ProxyHttp::proxy_upstream_filter`] rather than `request_filter`:
//!
//! ```text
//! request_filter        classify, resolve route
//! request_cache_filter  enable the cache if this route may reuse
//! cache lookup          hit, or wait on the cache lock as a coalesced follower
//! proxy_upstream_filter admission — only reached on a genuine miss
//! upstream_peer         pick a backend
//! ```
//!
//! Putting admission earlier would make cache hits and coalesced followers
//! queue for origin capacity they never consume. Pingora documents this hook
//! for exactly this purpose: "deferring checks like rate limiting ... to when
//! they are actually needed after cache miss".

use std::sync::Arc;
use std::time::Instant;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use pingora_cache::key::CacheKey as PingoraCacheKey;
use pingora_cache::{CacheMeta, CacheOptionOverrides, NoCacheReason, RespCacheable};
use pingora_core::prelude::*;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::{ProxyHttp, Session};

use crate::admission::limiter::{Limiter, Permit};
use crate::admission::{Admission, AdmissionController};
use crate::cache::policy::{Disposition, Shareability, evaluate_request, evaluate_response};
use crate::cache::{BoundedStore, KeyBuilder};
use crate::classifier::{FrameworkAdapter, RequestClass, RequestMetadata, nextjs::NextJs};
use crate::config::schema::{LogFormat, RouteCache, Timeouts};
use crate::policy::PolicySnapshot;
use crate::telemetry::logging::AccessLog;
use crate::telemetry::metrics;
use crate::upstream::UpstreamPool;

/// Per-request state.
pub struct Ctx {
    /// The policy generation this request started on. Pinned here so a reload
    /// mid-request cannot apply half of itself to one request.
    pub policy: Arc<PolicySnapshot>,
    pub started: Instant,
    pub class: RequestClass,
    pub route_id: Option<String>,
    /// Origin capacity, held for as long as this request is consuming it.
    ///
    /// Known gap: because the body is streamed straight through, this is
    /// released when the *client* finishes reading, not when the origin
    /// finishes rendering. A slow reader therefore occupies a slot sized for a
    /// render. The fix is a bounded decoupling buffer plus the downstream write
    /// timeout; until then `timeouts.downstream_write` is what bounds it.
    pub permit: Option<Permit>,
    pub shed: bool,
    pub upstream: Option<String>,
    pub origin_started: Option<Instant>,
    pub origin_finished_ms: Option<u128>,

    /// Cache policy resolved during `request_filter` and consumed later in the
    /// pipeline, where the request header is no longer convenient to re-read.
    pub route_cache: Option<RouteCache>,
    pub key_headers: Vec<String>,
    pub transient_only: bool,
    pub cache_active: bool,
    pub may_coalesce: bool,
    pub coalesce_override: bool,
    /// Where the origin permit was given back, for the access log.
    pub permit_released_at: Option<&'static str>,
}

impl Ctx {
    fn new(policy: Arc<PolicySnapshot>) -> Self {
        Ctx {
            policy,
            started: Instant::now(),
            class: RequestClass::Unknown,
            route_id: None,
            permit: None,
            shed: false,
            upstream: None,
            origin_started: None,
            origin_finished_ms: None,
            route_cache: None,
            key_headers: Vec::new(),
            transient_only: false,
            cache_active: false,
            may_coalesce: false,
            coalesce_override: false,
            permit_released_at: None,
        }
    }
}

pub struct Harmost {
    policy: Arc<ArcSwap<PolicySnapshot>>,
    admission: Arc<AdmissionController>,
    upstreams: Arc<UpstreamPool>,
    adapter: Arc<dyn FrameworkAdapter>,
    store: &'static BoundedStore,
    /// Built once at startup. Pingora's lock constructor controls how long a
    /// writer may own a key; follower wait time is configured per request.
    cache_lock: &'static pingora_cache::lock::CacheKeyLockImpl,
}

impl Harmost {
    pub fn new(
        policy: Arc<ArcSwap<PolicySnapshot>>,
        admission: Arc<AdmissionController>,
        upstreams: Arc<UpstreamPool>,
    ) -> Self {
        let initial = policy.load();
        // `Storage` takes `&'static self` throughout, so the store and the
        // lock are created once and leaked deliberately at startup.
        let store = BoundedStore::new(initial.config.cache.max_memory.get() as usize);
        let cache_lock: &'static pingora_cache::lock::CacheKeyLockImpl = Box::leak(
            pingora_cache::lock::CacheLock::new_boxed(initial.config.timeouts.origin.as_duration()),
        );
        drop(initial);

        Harmost {
            store,
            cache_lock,
            adapter: Arc::new(NextJs),
            upstreams,
            admission,
            policy,
        }
    }

    /// Route limiter for this request, created on first use.
    fn limiter_for(&self, policy: &PolicySnapshot, route_id: &str) -> Option<Arc<Limiter>> {
        let route = policy.routes.iter().find(|r| r.id == route_id)?;
        let c = route.config.concurrency.as_ref()?;
        Some(self.admission.route_limiter(
            route_id,
            c.max,
            c.queue.max,
            c.queue.timeout.as_duration(),
        ))
    }

    async fn refuse(&self, session: &mut Session, policy: &PolicySnapshot) -> Result<()> {
        let overload = &policy.config.overload;
        let mut resp = ResponseHeader::build(overload.status, Some(3))?;
        let retry_after = overload.retry_after.as_duration().as_secs().max(1);
        resp.insert_header("Retry-After", retry_after.to_string())?;
        // A CDN that caches this turns a brief origin blip into a long outage.
        resp.insert_header("Cache-Control", "no-store")?;
        if policy.config.debug_headers {
            resp.insert_header("X-Harmost", "SHED")?;
        }
        // proxy_upstream_filter(false) is reusable by default in Pingora. The
        // request body may still be unread, so keeping this connection alive
        // could make those bytes look like the next request.
        session.as_downstream_mut().set_keepalive(None);
        session.write_response_header(Box::new(resp), true).await?;
        Ok(())
    }
}

#[async_trait]
impl ProxyHttp for Harmost {
    type CTX = Ctx;

    fn new_ctx(&self) -> Ctx {
        // Pin the generation for the whole request.
        Ctx::new(self.policy.load_full())
    }

    async fn early_request_filter(&self, session: &mut Session, ctx: &mut Ctx) -> Result<()> {
        // Bound how long a single downstream write may block.
        //
        // This is what keeps a deliberately slow reader from occupying an
        // origin permit indefinitely on a response too large to fit in the
        // buffers between origin and client. It does not eliminate the
        // coupling — see the slow-reader note in the README — but it turns an
        // unbounded hold into a bounded one.
        let timeout = ctx.policy.config.timeouts.downstream_write.as_duration();
        if !timeout.is_zero() {
            session.as_downstream_mut().set_write_timeout(Some(timeout));
        }
        Ok(())
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Ctx) -> Result<bool> {
        let req = session.req_header();
        let host = req
            .headers
            .get("host")
            .and_then(|h| h.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let path = req.uri.path().to_string();
        let query = req.uri.query().map(str::to_string);
        let method = req.method.clone();

        let meta = RequestMetadata {
            method: &method,
            host: &host,
            path: &path,
            query: query.as_deref(),
            headers: &req.headers,
        };

        let hints = self.adapter.classify_request(&meta);
        let policy = ctx.policy.clone();
        let route = policy.resolve(&host, &path, &method);

        // A route's declared class outranks what the classifier inferred —
        // the operator knows things about their own routes that headers do not
        // reveal. It cannot make a private thing public: validation refuses
        // that combination at startup.
        ctx.class = route
            .and_then(|r| r.declared_class())
            .or(hints.class)
            .unwrap_or(RequestClass::Unknown);
        ctx.route_id = route.map(|r| r.id.clone());

        ctx.route_cache = route.and_then(|r| r.config.cache.clone());
        ctx.key_headers = resolved_key_headers(&hints.key_headers, ctx.route_cache.as_ref());
        let coalesce_override = route
            .and_then(|r| r.config.coalesce.as_ref())
            .is_some_and(|c| c.override_origin);
        ctx.coalesce_override = coalesce_override;
        let route_cache_enabled = ctx.policy.config.cache.enabled
            && ctx
                .route_cache
                .as_ref()
                .and_then(|cache| cache.enabled)
                .unwrap_or(true);
        let route_coalesce_enabled = route
            .and_then(|r| r.config.coalesce.as_ref())
            .and_then(|coalesce| coalesce.enabled)
            .unwrap_or(ctx.policy.config.coalesce.enabled);

        let route_label = ctx.route_id.as_deref().unwrap_or("-");
        metrics::REQUESTS
            .with_label_values(&[route_label, ctx.class.as_str()])
            .inc();

        let disposition = evaluate_request(
            &meta,
            ctx.class,
            hints.force_bypass,
            ctx.route_cache.as_ref(),
            coalesce_override,
            route_cache_enabled,
            route_coalesce_enabled,
        );
        if let Disposition::Eligible {
            mut may_store,
            may_coalesce,
        } = disposition
        {
            if hints.coalesce_only {
                may_store = false;
            }
            ctx.cache_active = may_store || may_coalesce;
            ctx.transient_only = !may_store;
            ctx.may_coalesce = may_coalesce;
        }

        // The denominator of the origin-work-avoidance ratio, counted only
        // where reuse was genuinely possible.
        if ctx.cache_active {
            metrics::REUSE_ELIGIBLE
                .with_label_values(&[route_label])
                .inc();
        }

        Ok(false)
    }

    fn request_cache_filter(&self, session: &mut Session, ctx: &mut Ctx) -> Result<()> {
        if !ctx.cache_active {
            return Ok(());
        }
        let lock_options = ctx.may_coalesce.then(|| {
            let mut options = CacheOptionOverrides::default();
            options.wait_timeout = Some(ctx.policy.coalesce_wait());
            options
        });
        session.cache.enable(
            self.store,
            None,
            None,
            ctx.may_coalesce.then_some(self.cache_lock),
            lock_options,
        );
        // Upstream tracks body bytes and marks the response uncacheable past
        // this limit, so an oversized body streams to the client without
        // being retained.
        session
            .cache
            .set_max_file_size_bytes(ctx.policy.config.cache.max_body_size.get() as usize);
        Ok(())
    }

    fn cache_key_callback(&self, session: &Session, ctx: &mut Ctx) -> Result<PingoraCacheKey> {
        let req = session.req_header();
        let host = req
            .headers
            .get("host")
            .and_then(|h| h.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let path = req.uri.path().to_string();
        let query = req.uri.query().map(str::to_string);

        let meta = RequestMetadata {
            method: &req.method,
            host: &host,
            path: &path,
            query: query.as_deref(),
            headers: &req.headers,
        };

        let key = KeyBuilder {
            scheme: "http",
            query_policy: ctx.route_cache.as_ref().and_then(|c| c.query.as_ref()),
            variant_headers: &ctx.key_headers,
            deployment: ctx.policy.config.deployment.id.as_deref(),
        }
        .build(&meta);

        Ok(PingoraCacheKey::new("", key.canonical_string(), ""))
    }

    /// Admission. Reached only on a genuine cache miss, so hits and coalesced
    /// followers never consume origin capacity.
    async fn proxy_upstream_filter(&self, session: &mut Session, ctx: &mut Ctx) -> Result<bool> {
        if matches!(
            session.cache.phase(),
            pingora_cache::CachePhase::Disabled(NoCacheReason::CacheLockTimeout)
        ) && ctx.policy.config.coalesce.on_timeout
            == crate::config::schema::OnCoalesceTimeout::StaleOrShed
        {
            ctx.shed = true;
            self.refuse(session, &ctx.policy).await?;
            return Ok(false);
        }
        let route_limiter = ctx
            .route_id
            .as_deref()
            .and_then(|id| self.limiter_for(&ctx.policy, id));
        let route_label = ctx.route_id.as_deref().unwrap_or("-").to_string();
        let outcome = self
            .admission
            .admit(ctx.class, route_limiter.as_ref())
            .await;

        // Publish limiter state on the way through rather than from a timer:
        // these are the numbers an operator wants during an incident, and a
        // sampled gauge would miss the spike that caused it.
        let global = self.admission.global();
        metrics::LIMIT
            .with_label_values(&["global"])
            .set(global.limit() as i64);
        metrics::QUEUE_DEPTH
            .with_label_values(&["global"])
            .set(global.queue_depth() as i64);
        metrics::IN_FLIGHT
            .with_label_values(&["global"])
            .set((global.limit().saturating_sub(global.available())) as i64);
        if let Some(l) = &route_limiter {
            metrics::LIMIT
                .with_label_values(&[l.name()])
                .set(l.limit() as i64);
            metrics::QUEUE_DEPTH
                .with_label_values(&[l.name()])
                .set(l.queue_depth() as i64);
            metrics::IN_FLIGHT
                .with_label_values(&[l.name()])
                .set((l.limit().saturating_sub(l.available())) as i64);
        }

        match outcome {
            Admission::Admitted(permits) => {
                ctx.permit = Some(permits.into_inner());
                metrics::ADMISSION
                    .with_label_values(&[&route_label, "admitted"])
                    .inc();
                Ok(true)
            }
            Admission::Exempt => {
                metrics::ADMISSION
                    .with_label_values(&[&route_label, "exempt"])
                    .inc();
                Ok(true)
            }
            Admission::Shed(reason) => {
                ctx.shed = true;
                metrics::ADMISSION
                    .with_label_values(&[&route_label, &format!("shed_{}", reason.as_str())])
                    .inc();
                self.refuse(session, &ctx.policy).await?;
                Ok(false)
            }
        }
    }

    /// Whether a stale entry may be served.
    ///
    /// Pingora only calls this once it has already confirmed the entry is
    /// inside its stale window, so the window itself comes from `CacheMeta` —
    /// which is to say from the route's `stale_while_revalidate` /
    /// `stale_if_error`, falling back to whatever the origin asked for. This
    /// is purely the policy question of whether to use it.
    ///
    /// The default implementation answers `false` when `error` is `None`,
    /// which is precisely the stale-while-revalidate path — so without this
    /// override, configuring `stale_while_revalidate` would quietly do
    /// nothing.
    fn should_serve_stale(
        &self,
        _session: &mut Session,
        _ctx: &mut Ctx,
        error: Option<&Error>,
    ) -> bool {
        match error {
            // Background revalidation in flight: serve the stale copy now.
            None => true,
            // Only an origin failure justifies stale. An error raised by
            // Harmost itself (a shed, a bad config) is not a reason to hand
            // out old content.
            Some(e) => e.esource() == &pingora_core::ErrorSource::Upstream,
        }
    }

    fn response_cache_filter(
        &self,
        _session: &Session,
        resp: &ResponseHeader,
        ctx: &mut Ctx,
    ) -> Result<RespCacheable> {
        let cache_control = joined_header_values(&resp.headers, http::header::CACHE_CONTROL);
        let vary = joined_header_values(&resp.headers, http::header::VARY);
        let meta = crate::cache::policy::ResponseMetadata {
            status: resp.status.as_u16(),
            cache_control: cache_control.as_deref(),
            set_cookie: resp.headers.contains_key("set-cookie"),
            vary: vary.as_deref(),
        };

        match evaluate_response(
            &meta,
            ctx.route_cache.as_ref(),
            &ctx.key_headers,
            ctx.transient_only,
            ctx.coalesce_override,
        ) {
            Shareability::Shareable { ttl, swr, sie } => {
                let now = std::time::SystemTime::now();
                let fresh_until = now.checked_add(ttl).ok_or_else(|| {
                    Error::explain(
                        ErrorType::InternalError,
                        "cache TTL exceeds SystemTime range",
                    )
                })?;
                let mut stored = resp.clone();
                // Never let an origin-supplied value collide with Harmost's
                // private storage marker.
                stored.remove_header(crate::cache::TRANSIENT_HEADER);
                Ok(RespCacheable::Cacheable(CacheMeta::new(
                    fresh_until,
                    now,
                    swr.as_secs().min(u32::MAX as u64) as u32,
                    sie.as_secs().min(u32::MAX as u64) as u32,
                    stored,
                )))
            }
            // Collapse the in-flight herd onto one render, retain nothing: an
            // entry born stale is served to everyone already waiting on the
            // lock and to nobody afterwards.
            Shareability::TransientOnly => {
                let now = std::time::SystemTime::now();
                let freshness = ctx
                    .policy
                    .coalesce_wait()
                    .max(std::time::Duration::from_millis(1));
                let fresh_until = now.checked_add(freshness).ok_or_else(|| {
                    Error::explain(
                        ErrorType::InternalError,
                        "coalesce timeout exceeds SystemTime range",
                    )
                })?;
                let mut transient = resp.clone();
                transient.remove_header(crate::cache::TRANSIENT_HEADER);
                transient.insert_header(crate::cache::TRANSIENT_HEADER, "1")?;
                Ok(RespCacheable::Cacheable(CacheMeta::new(
                    fresh_until,
                    now,
                    0,
                    0,
                    transient,
                )))
            }
            Shareability::NotShareable(reason) => {
                metrics::BYPASS_REASON
                    .with_label_values(&[ctx.route_id.as_deref().unwrap_or("-"), reason.as_str()])
                    .inc();
                Ok(RespCacheable::Uncacheable(NoCacheReason::OriginNotCache))
            }
        }
    }

    async fn upstream_peer(&self, session: &mut Session, ctx: &mut Ctx) -> Result<Box<HttpPeer>> {
        let path = session.req_header().uri.path();
        let backend = self
            .upstreams
            .select(path)
            .ok_or_else(|| Error::explain(ErrorType::InternalError, "no upstream configured"))?;
        ctx.upstream = Some(backend.address.clone());
        ctx.origin_started = Some(Instant::now());
        metrics::ORIGIN_REQUESTS
            .with_label_values(&[ctx.route_id.as_deref().unwrap_or("-"), &backend.address])
            .inc();
        let mut peer = HttpPeer::new(backend.socket, false, String::new());
        configure_peer_timeouts(&mut peer, &ctx.policy.config.timeouts);
        Ok(Box::new(peer))
    }

    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        upstream: &mut RequestHeader,
        _ctx: &mut Ctx,
    ) -> Result<()> {
        if let Some(peer) = session.client_addr().and_then(|address| address.as_inet()) {
            upstream.append_header("X-Forwarded-For", peer.ip().to_string())?;
        }
        upstream.insert_header("X-Forwarded-Proto", "http")?;
        Ok(())
    }

    async fn upstream_response_filter(
        &self,
        _session: &mut Session,
        _upstream: &mut ResponseHeader,
        ctx: &mut Ctx,
    ) -> Result<()> {
        check_origin_deadline(ctx)
    }

    fn upstream_response_body_filter(
        &self,
        _session: &mut Session,
        _body: &mut Option<bytes::Bytes>,
        end_of_stream: bool,
        ctx: &mut Ctx,
    ) -> Result<Option<std::time::Duration>> {
        check_origin_deadline(ctx)?;
        if end_of_stream {
            ctx.origin_finished_ms = ctx
                .origin_started
                .map(|started| started.elapsed().as_millis());
            if ctx.permit.is_some() {
                ctx.permit_released_at = Some("body_end");
            }
            // The permit represents *render* capacity, and this is the
            // earliest point at which the origin is known to have stopped
            // rendering. It is not necessarily the moment it did: pingora
            // paces upstream reads against downstream writes, so a slow reader
            // delays when `end_of_stream` is observed. That gap is bounded by
            // `timeouts.downstream_write` and closed properly only by a
            // bounded response spool (roadmap phase 1).
            //
            // Releasing earlier on the strength of a `Content-Length` was
            // tried and reverted: a length describes the body's size, never
            // that the origin has finished producing it. See
            // `bench/slowclient.sh`.
            ctx.permit = None;
        }
        Ok(None)
    }

    async fn response_filter(
        &self,
        session: &mut Session,
        resp: &mut ResponseHeader,
        ctx: &mut Ctx,
    ) -> Result<()> {
        resp.remove_header(crate::cache::TRANSIENT_HEADER);
        if ctx.policy.config.debug_headers {
            resp.insert_header("X-Harmost", cache_status(session, ctx).to_ascii_uppercase())?;
        }
        Ok(())
    }

    async fn logging(&self, session: &mut Session, _e: Option<&Error>, ctx: &mut Ctx) {
        let status = session
            .response_written()
            .map(|r| r.status.as_u16())
            .unwrap_or(0);
        let route = ctx.route_id.as_deref().unwrap_or("-");
        let cache = cache_status(session, ctx);
        metrics::CACHE.with_label_values(&[route, cache]).inc();

        let origin_ms = match (ctx.origin_finished_ms, ctx.origin_started) {
            (Some(elapsed_ms), _) => {
                metrics::ORIGIN_LATENCY
                    .with_label_values(&[route])
                    .observe(elapsed_ms as f64 / 1000.0);
                elapsed_ms
            }
            (None, Some(started)) => {
                let elapsed = started.elapsed();
                metrics::ORIGIN_LATENCY
                    .with_label_values(&[route])
                    .observe(elapsed.as_secs_f64());
                elapsed.as_millis()
            }
            (None, None) => 0,
        };

        let access = AccessLog {
            method: session.req_header().method.as_str(),
            // Path only — the query string routinely carries tokens.
            path: session.req_header().uri.path(),
            route,
            class: ctx.class.as_str(),
            cache,
            upstream: ctx.upstream.as_deref(),
            status,
            shed: ctx.shed,
            origin_ms,
            total_ms: ctx.started.elapsed().as_millis(),
            permit_released_at: ctx.permit_released_at.unwrap_or("-"),
        };
        let line = match ctx.policy.config.telemetry.logging.format {
            LogFormat::Json => access.to_json(),
            LogFormat::Text => access.to_text(),
        };
        log::info!("{line}");

        // Normally already released at upstream end-of-stream; this covers
        // the paths that never got there (shed, cache hit, upstream error).
        // Then republish the gauges: updating them only on the way in leaves
        // `in_flight` reading stale while the proxy is idle, which is the
        // moment an operator is most likely to be looking at it.
        ctx.permit = None;
        let global = self.admission.global();
        metrics::IN_FLIGHT
            .with_label_values(&["global"])
            .set((global.limit().saturating_sub(global.available())) as i64);
        metrics::QUEUE_DEPTH
            .with_label_values(&["global"])
            .set(global.queue_depth() as i64);
        if let Some(l) = ctx
            .route_id
            .as_deref()
            .and_then(|id| self.limiter_for(&ctx.policy, id))
        {
            metrics::IN_FLIGHT
                .with_label_values(&[l.name()])
                .set((l.limit().saturating_sub(l.available())) as i64);
            metrics::QUEUE_DEPTH
                .with_label_values(&[l.name()])
                .set(l.queue_depth() as i64);
        }
    }
}

fn nonzero(duration: std::time::Duration) -> Option<std::time::Duration> {
    (!duration.is_zero()).then_some(duration)
}

fn configure_peer_timeouts(peer: &mut HttpPeer, timeouts: &Timeouts) {
    peer.options.connection_timeout = nonzero(timeouts.connect.as_duration());
    peer.options.total_connection_timeout = nonzero(timeouts.connect.as_duration());
    peer.options.idle_timeout = nonzero(timeouts.idle.as_duration());
    peer.options.read_timeout = [
        timeouts.first_byte.as_duration(),
        timeouts.idle.as_duration(),
        timeouts.origin.as_duration(),
    ]
    .into_iter()
    .filter(|duration| !duration.is_zero())
    .min();
}

fn resolved_key_headers(framework: &[&str], route: Option<&RouteCache>) -> Vec<String> {
    let mut headers = framework
        .iter()
        .map(|header| (*header).to_string())
        .collect::<Vec<_>>();
    if let Some(vary) = route.and_then(|cache| cache.vary.as_ref()) {
        headers.extend(vary.headers.iter().cloned());
    }
    for header in &mut headers {
        header.make_ascii_lowercase();
    }
    headers.sort_unstable();
    headers.dedup();
    headers
}

fn check_origin_deadline(ctx: &Ctx) -> Result<()> {
    let timeout = ctx.policy.config.timeouts.origin.as_duration();
    if !timeout.is_zero()
        && ctx
            .origin_started
            .is_some_and(|started| started.elapsed() >= timeout)
    {
        return Err(Error::create(
            ErrorType::ReadTimedout,
            pingora_core::ErrorSource::Upstream,
            None,
            Some("origin request exceeded timeouts.origin".into()),
        ));
    }
    Ok(())
}

/// One vocabulary for the cache outcome, shared by the debug header, the
/// access log and the metrics, so the three can never disagree.
fn cache_status(session: &Session, ctx: &Ctx) -> &'static str {
    match session.cache.phase() {
        pingora_cache::CachePhase::Hit => "hit",
        pingora_cache::CachePhase::Stale | pingora_cache::CachePhase::StaleUpdating => "stale",
        pingora_cache::CachePhase::Miss | pingora_cache::CachePhase::Expired => "miss",
        pingora_cache::CachePhase::Bypass => "bypass",
        _ if ctx.shed => "shed",
        _ if ctx.cache_active => "miss",
        _ => "disabled",
    }
}

fn joined_header_values(
    headers: &http::HeaderMap,
    name: http::header::HeaderName,
) -> Option<String> {
    let values = headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect::<Vec<_>>();
    (!values.is_empty()).then(|| values.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::VaryPolicy;

    #[test]
    fn route_vary_headers_are_merged_with_framework_variants() {
        let route = RouteCache {
            vary: Some(VaryPolicy {
                headers: vec!["Accept-Language".into(), "RSC".into()],
            }),
            ..Default::default()
        };
        assert_eq!(
            resolved_key_headers(&["rsc", "next-url"], Some(&route)),
            ["accept-language", "next-url", "rsc"]
        );
    }

    #[test]
    fn repeated_cache_policy_headers_are_combined() {
        let mut headers = http::HeaderMap::new();
        headers.append(
            http::header::CACHE_CONTROL,
            "public, max-age=60".parse().unwrap(),
        );
        headers.append(http::header::CACHE_CONTROL, "private".parse().unwrap());
        assert_eq!(
            joined_header_values(&headers, http::header::CACHE_CONTROL).as_deref(),
            Some("public, max-age=60,private")
        );
    }

    #[test]
    fn every_origin_timeout_is_applied_to_the_peer() {
        let timeouts = Timeouts::default();
        let mut peer = HttpPeer::new("127.0.0.1:3000", false, String::new());
        configure_peer_timeouts(&mut peer, &timeouts);
        assert_eq!(
            peer.options.connection_timeout,
            Some(timeouts.connect.as_duration())
        );
        assert_eq!(
            peer.options.total_connection_timeout,
            Some(timeouts.connect.as_duration())
        );
        assert_eq!(peer.options.idle_timeout, Some(timeouts.idle.as_duration()));
        assert_eq!(
            peer.options.read_timeout,
            Some(timeouts.first_byte.as_duration())
        );
    }
}
