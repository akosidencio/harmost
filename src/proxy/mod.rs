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

use async_trait::async_trait;
use pingora_core::prelude::*;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_cache::key::CacheKey as PingoraCacheKey;
use pingora_cache::{CacheMeta, NoCacheReason, RespCacheable};
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::{ProxyHttp, Session};

use crate::admission::limiter::{Limiter, Permit};
use crate::admission::{Admission, AdmissionController};
use crate::cache::policy::{Disposition, Shareability, evaluate_request, evaluate_response};
use crate::cache::{BoundedStore, KeyBuilder};
use crate::classifier::{FrameworkAdapter, RequestClass, RequestMetadata, nextjs::NextJs};
use crate::config::schema::RouteCache;
use crate::policy::PolicySnapshot;
use crate::telemetry::logging::AccessLog;
use crate::telemetry::metrics;
use crate::upstream::UpstreamPool;

/// Per-request state.
pub struct Ctx {
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

    /// Cache policy resolved during `request_filter` and consumed later in the
    /// pipeline, where the request header is no longer convenient to re-read.
    pub route_cache: Option<RouteCache>,
    pub key_headers: Vec<&'static str>,
    pub coalesce_only: bool,
    pub may_store: bool,
}

impl Ctx {
    fn new() -> Self {
        Ctx {
            started: Instant::now(),
            class: RequestClass::Unknown,
            route_id: None,
            permit: None,
            shed: false,
            upstream: None,
            origin_started: None,
            route_cache: None,
            key_headers: Vec::new(),
            coalesce_only: false,
            may_store: false,
        }
    }
}

pub struct Harmost {
    policy: Arc<PolicySnapshot>,
    admission: Arc<AdmissionController>,
    upstreams: Arc<UpstreamPool>,
    adapter: Arc<dyn FrameworkAdapter>,
    store: &'static BoundedStore,
    cache_lock: &'static pingora_cache::lock::CacheKeyLockImpl,
    max_body_size: usize,
    cache_enabled: bool,
    debug_headers: bool,
    overload_status: u16,
    retry_after: u64,
}

impl Harmost {
    pub fn new(
        policy: Arc<PolicySnapshot>,
        admission: Arc<AdmissionController>,
        upstreams: Arc<UpstreamPool>,
    ) -> Self {
        // `Storage` takes `&'static self` throughout, so the store and the
        // lock are created once and leaked deliberately at startup.
        let store = BoundedStore::new(policy.config.cache.max_memory.get() as usize);
        let cache_lock: &'static pingora_cache::lock::CacheKeyLockImpl = Box::leak(
            pingora_cache::lock::CacheLock::new_boxed(policy.coalesce_wait()),
        );

        Harmost {
            store,
            cache_lock,
            max_body_size: policy.config.cache.max_body_size.get() as usize,
            cache_enabled: policy.config.cache.enabled,
            debug_headers: policy.config.debug_headers,
            overload_status: policy.config.overload.status,
            retry_after: policy.config.overload.retry_after.as_duration().as_secs().max(1),
            adapter: Arc::new(NextJs),
            upstreams,
            admission,
            policy,
        }
    }

    /// Route limiter for this request, created on first use.
    fn limiter_for(&self, route_id: &str) -> Option<Arc<Limiter>> {
        let route = self.policy.routes.iter().find(|r| r.id == route_id)?;
        let c = route.config.concurrency.as_ref()?;
        Some(self.admission.route_limiter(
            route_id,
            c.max,
            c.queue.max,
            c.queue.timeout.as_duration(),
        ))
    }

    async fn refuse(&self, session: &mut Session) -> Result<()> {
        let mut resp = ResponseHeader::build(self.overload_status, Some(3))?;
        resp.insert_header("Retry-After", self.retry_after.to_string())?;
        // A CDN that caches this turns a brief origin blip into a long outage.
        resp.insert_header("Cache-Control", "no-store")?;
        if self.debug_headers {
            resp.insert_header("X-Harmost", "SHED")?;
        }
        session.write_response_header(Box::new(resp), true).await?;
        Ok(())
    }
}

#[async_trait]
impl ProxyHttp for Harmost {
    type CTX = Ctx;

    fn new_ctx(&self) -> Ctx {
        Ctx::new()
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
        let route = self.policy.resolve(&host, &path, &method);

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
        ctx.key_headers = hints.key_headers.clone();
        ctx.coalesce_only = hints.coalesce_only;

        let coalesce_override = route
            .and_then(|r| r.config.coalesce.as_ref())
            .is_some_and(|c| c.override_origin);

        let route_label = ctx.route_id.as_deref().unwrap_or("-");
        metrics::REQUESTS.with_label_values(&[route_label, ctx.class.as_str()]).inc();

        ctx.may_store = self.cache_enabled
            && matches!(
                evaluate_request(
                    &meta,
                    ctx.class,
                    hints.force_bypass,
                    ctx.route_cache.as_ref(),
                    coalesce_override,
                ),
                Disposition::Eligible { .. }
            );

        // The denominator of the origin-work-avoidance ratio, counted only
        // where reuse was genuinely possible.
        if ctx.may_store {
            metrics::REUSE_ELIGIBLE.with_label_values(&[route_label]).inc();
        }

        Ok(false)
    }

    fn request_cache_filter(&self, session: &mut Session, ctx: &mut Ctx) -> Result<()> {
        if !ctx.may_store {
            return Ok(());
        }
        session
            .cache
            .enable(self.store, None, None, Some(self.cache_lock), None);
        // Upstream tracks body bytes and marks the response uncacheable past
        // this limit, so an oversized body streams to the client without
        // being retained.
        session.cache.set_max_file_size_bytes(self.max_body_size);
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

        let variant: Vec<&str> = ctx.key_headers.to_vec();
        let key = KeyBuilder {
            scheme: "http",
            query_policy: ctx.route_cache.as_ref().and_then(|c| c.query.as_ref()),
            variant_headers: &variant,
            deployment: self.policy.config.deployment.id.as_deref(),
        }
        .build(&meta);

        Ok(PingoraCacheKey::new("", key.canonical_string(), ""))
    }

    /// Admission. Reached only on a genuine cache miss, so hits and coalesced
    /// followers never consume origin capacity.
    async fn proxy_upstream_filter(&self, session: &mut Session, ctx: &mut Ctx) -> Result<bool> {
        let route_limiter = ctx.route_id.as_deref().and_then(|id| self.limiter_for(id));
        let route_label = ctx.route_id.as_deref().unwrap_or("-").to_string();
        let outcome = self.admission.admit(ctx.class, route_limiter.as_ref()).await;

        // Publish limiter state on the way through rather than from a timer:
        // these are the numbers an operator wants during an incident, and a
        // sampled gauge would miss the spike that caused it.
        let global = self.admission.global();
        metrics::LIMIT.with_label_values(&["global"]).set(global.limit() as i64);
        metrics::QUEUE_DEPTH.with_label_values(&["global"]).set(global.queue_depth() as i64);
        metrics::IN_FLIGHT
            .with_label_values(&["global"])
            .set((global.limit().saturating_sub(global.available())) as i64);
        if let Some(l) = &route_limiter {
            metrics::LIMIT.with_label_values(&[l.name()]).set(l.limit() as i64);
            metrics::QUEUE_DEPTH.with_label_values(&[l.name()]).set(l.queue_depth() as i64);
            metrics::IN_FLIGHT
                .with_label_values(&[l.name()])
                .set((l.limit().saturating_sub(l.available())) as i64);
        }

        match outcome {
            Admission::Admitted(permits) => {
                ctx.permit = Some(permits.into_inner());
                metrics::ADMISSION.with_label_values(&[&route_label, "admitted"]).inc();
                Ok(true)
            }
            Admission::Exempt => {
                metrics::ADMISSION.with_label_values(&[&route_label, "exempt"]).inc();
                Ok(true)
            }
            Admission::Shed(reason) => {
                ctx.shed = true;
                metrics::ADMISSION
                    .with_label_values(&[&route_label, &format!("shed_{}", reason.as_str())])
                    .inc();
                self.refuse(session).await?;
                Ok(false)
            }
        }
    }

    fn response_cache_filter(
        &self,
        _session: &Session,
        resp: &ResponseHeader,
        ctx: &mut Ctx,
    ) -> Result<RespCacheable> {
        let meta = crate::cache::policy::ResponseMetadata {
            status: resp.status.as_u16(),
            cache_control: resp.headers.get("cache-control").and_then(|v| v.to_str().ok()),
            set_cookie: resp.headers.contains_key("set-cookie"),
            vary: resp.headers.get("vary").and_then(|v| v.to_str().ok()),
        };

        match evaluate_response(&meta, ctx.route_cache.as_ref(), &ctx.key_headers, ctx.coalesce_only) {
            Shareability::Shareable { ttl, swr, sie } => {
                let now = std::time::SystemTime::now();
                Ok(RespCacheable::Cacheable(CacheMeta::new(
                    now + ttl,
                    now,
                    swr.as_secs() as u32,
                    sie.as_secs() as u32,
                    resp.clone(),
                )))
            }
            // Collapse the in-flight herd onto one render, retain nothing: an
            // entry born stale is served to everyone already waiting on the
            // lock and to nobody afterwards.
            Shareability::TransientOnly => {
                let now = std::time::SystemTime::now();
                Ok(RespCacheable::Cacheable(CacheMeta::new(now, now, 0, 0, resp.clone())))
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
        Ok(Box::new(HttpPeer::new(backend.address.as_str(), false, String::new())))
    }

    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        upstream: &mut RequestHeader,
        _ctx: &mut Ctx,
    ) -> Result<()> {
        if let Some(peer) = session.client_addr() {
            upstream.insert_header("X-Forwarded-For", peer.to_string())?;
        }
        upstream.insert_header("X-Forwarded-Proto", "http")?;
        Ok(())
    }

    fn upstream_response_body_filter(
        &self,
        _session: &mut Session,
        _body: &mut Option<bytes::Bytes>,
        end_of_stream: bool,
        ctx: &mut Ctx,
    ) -> Result<Option<std::time::Duration>> {
        if end_of_stream {
            // The permit represents *render* capacity, so it is returned the
            // moment the origin stops rendering. Holding it until the client
            // finished reading would let a deliberately slow reader occupy a
            // slot sized for a 200ms render, which is a cheap way to defeat
            // the limiter.
            //
            // Residual coupling remains: Pingora paces upstream reads against
            // downstream writes, so a slow client can still stretch how long
            // the origin takes to drain. `timeouts.downstream_write` is what
            // bounds that.
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
        if self.debug_headers {
            resp.insert_header("X-Harmost", cache_status(session, ctx).to_ascii_uppercase())?;
        }
        Ok(())
    }

    async fn logging(&self, session: &mut Session, _e: Option<&Error>, ctx: &mut Ctx) {
        let status = session.response_written().map(|r| r.status.as_u16()).unwrap_or(0);
        let route = ctx.route_id.as_deref().unwrap_or("-");
        let cache = cache_status(session, ctx);
        metrics::CACHE.with_label_values(&[route, cache]).inc();

        let origin_ms = match ctx.origin_started {
            Some(started) => {
                let elapsed = started.elapsed();
                metrics::ORIGIN_LATENCY
                    .with_label_values(&[route])
                    .observe(elapsed.as_secs_f64());
                elapsed.as_millis()
            }
            None => 0,
        };

        log::info!(
            "{}",
            AccessLog {
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
            }
            .to_json()
        );

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
        metrics::QUEUE_DEPTH.with_label_values(&["global"]).set(global.queue_depth() as i64);
        if let Some(l) = ctx.route_id.as_deref().and_then(|id| self.limiter_for(id)) {
            metrics::IN_FLIGHT
                .with_label_values(&[l.name()])
                .set((l.limit().saturating_sub(l.available())) as i64);
            metrics::QUEUE_DEPTH.with_label_values(&[l.name()]).set(l.queue_depth() as i64);
        }
    }
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
        _ if ctx.may_store => "miss",
        _ => "disabled",
    }
}
