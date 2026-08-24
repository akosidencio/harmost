//! The Pingora proxy layer.
//!
//! Order of operations in [`ProxyHttp::request_filter`] is the whole design:
//! classify, resolve policy, then admit. Reuse (cache, coalescing) is handled
//! by `pingora-cache` further along the pipeline and deliberately runs *before*
//! a request reaches the origin, because a hit consumes no origin capacity and
//! should never queue for it.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use pingora_core::prelude::*;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::{ProxyHttp, Session};

use crate::admission::limiter::{Limiter, Permit};
use crate::admission::{Admission, AdmissionController};
use crate::classifier::{FrameworkAdapter, RequestClass, RequestMetadata, nextjs::NextJs};
use crate::policy::PolicySnapshot;
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
        }
    }
}

pub struct Harmost {
    policy: Arc<PolicySnapshot>,
    admission: Arc<AdmissionController>,
    upstreams: Arc<UpstreamPool>,
    adapter: Arc<dyn FrameworkAdapter>,
    debug_headers: bool,
    overload_status: u16,
    retry_after: u64,
}

impl Harmost {
    pub fn new(policy: Arc<PolicySnapshot>, admission: Arc<AdmissionController>) -> Self {
        let upstreams = Arc::new(UpstreamPool::new(
            &policy.config.origin.upstreams,
            policy.config.origin.load_balancing,
        ));
        Harmost {
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

        let route_limiter = ctx.route_id.as_deref().and_then(|id| self.limiter_for(id));

        match self.admission.admit(ctx.class, route_limiter.as_ref()).await {
            Admission::Admitted(permits) => {
                ctx.permit = Some(permits.into_inner());
                Ok(false)
            }
            Admission::Exempt => Ok(false),
            Admission::Shed(_reason) => {
                ctx.shed = true;
                self.refuse(session).await?;
                // Handled here; the request never reaches the origin.
                Ok(true)
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

    async fn response_filter(
        &self,
        _session: &mut Session,
        resp: &mut ResponseHeader,
        ctx: &mut Ctx,
    ) -> Result<()> {
        if self.debug_headers {
            resp.insert_header("X-Harmost", if ctx.permit.is_some() { "MISS" } else { "PASS" })?;
        }
        Ok(())
    }

    async fn logging(&self, session: &mut Session, _e: Option<&Error>, ctx: &mut Ctx) {
        let status = session.response_written().map(|r| r.status.as_u16()).unwrap_or(0);
        let origin_ms = ctx.origin_started.map(|t| t.elapsed().as_millis()).unwrap_or(0);
        log::info!(
            r#"{{"path":"{}","route":"{}","class":"{}","shed":{},"upstream":"{}","origin_ms":{},"total_ms":{},"status":{}}}"#,
            session.req_header().uri.path(),
            ctx.route_id.as_deref().unwrap_or("-"),
            ctx.class.as_str(),
            ctx.shed,
            ctx.upstream.as_deref().unwrap_or("-"),
            origin_ms,
            ctx.started.elapsed().as_millis(),
            status,
        );
        // The permit drops with ctx, releasing origin capacity.
    }
}
