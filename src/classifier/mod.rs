//! What kind of request is this, and may its response ever be shared?

pub mod generic;
pub mod nextjs;

use http::{HeaderMap, Method, header};

/// Every request lands in exactly one class. The class decides the *defaults*;
/// a route policy may narrow them but never widen past what safety allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestClass {
    /// Fingerprinted assets. Long-lived, freely shareable.
    Static,
    /// A rendered public document. Cacheable and coalescible when the origin
    /// (or a fenced route override) permits.
    PublicDocument,
    /// Public but parameterised — search, filters, listings. Reuse only when a
    /// route says so, because the key space is attacker-influenced.
    PublicDynamic,
    /// Per-user. Never response-cached, never coalesced. Still admission
    /// controlled, which is most of the protection anyway.
    PrivateDynamic,
    /// State-changing. Never cached, coalesced, or retried.
    Mutation,
    /// Long-lived response bodies: SSE, streamed tokens. Exempt from the origin
    /// work permit, because holding one for the life of the connection would
    /// starve every other route.
    Streaming,
    /// Could not be classified. Passes through untouched.
    Unknown,
}

impl RequestClass {
    /// May a response in this class ever be stored, before any header is read?
    pub fn storable_in_principle(self) -> bool {
        matches!(self, RequestClass::Static | RequestClass::PublicDocument | RequestClass::PublicDynamic)
    }

    /// May concurrent equivalent requests in this class be collapsed?
    pub fn coalescible_in_principle(self) -> bool {
        matches!(self, RequestClass::PublicDocument | RequestClass::PublicDynamic)
    }

    /// Does a request in this class consume an origin work permit?
    ///
    /// Streaming is exempt by design: an SSE connection held for an hour would
    /// otherwise occupy a slot sized for a 200ms render.
    pub fn consumes_origin_permit(self) -> bool {
        !matches!(self, RequestClass::Static | RequestClass::Streaming)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            RequestClass::Static => "static",
            RequestClass::PublicDocument => "public_document",
            RequestClass::PublicDynamic => "public_dynamic",
            RequestClass::PrivateDynamic => "private_dynamic",
            RequestClass::Mutation => "mutation",
            RequestClass::Streaming => "streaming",
            RequestClass::Unknown => "unknown",
        }
    }
}

/// The request, reduced to what classification and keying actually read.
pub struct RequestMetadata<'a> {
    pub method: &'a Method,
    pub host: &'a str,
    pub path: &'a str,
    pub query: Option<&'a str>,
    pub headers: &'a HeaderMap,
}

impl<'a> RequestMetadata<'a> {
    pub fn header(&self, name: &str) -> Option<&'a str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    pub fn has_authorization(&self) -> bool {
        self.headers.contains_key(header::AUTHORIZATION)
    }

    pub fn has_cookies(&self) -> bool {
        self.headers.contains_key(header::COOKIE)
    }

    /// Does any `Cookie` header carry a cookie with this name?
    pub fn has_cookie_named(&self, name: &str) -> bool {
        self.headers.get_all(header::COOKIE).iter().any(|v| {
            v.to_str().is_ok_and(|raw| {
                raw.split(';').any(|pair| {
                    pair.split('=').next().map(str::trim).is_some_and(|k| k == name)
                })
            })
        })
    }

    pub fn is_safe_method(&self) -> bool {
        self.method == Method::GET || self.method == Method::HEAD
    }
}

/// What an adapter concluded about a request.
#[derive(Debug, Clone, Default)]
pub struct RequestHints {
    pub class: Option<RequestClass>,
    /// Headers this framework requires in the cache key, because they select
    /// between genuinely different response bodies at the same URL.
    pub key_headers: Vec<&'static str>,
    /// A framework-specific reason this response must never be shared.
    pub force_bypass: Option<&'static str>,
    /// Collapse concurrent duplicates, but never store the result. For
    /// responses that are safe to share within a single instant yet have
    /// unbounded key cardinality over time.
    pub coalesce_only: bool,
}

/// Framework-specific behaviour lives behind this trait and nowhere else.
pub trait FrameworkAdapter: Send + Sync {
    fn name(&self) -> &'static str;
    fn classify_request(&self, req: &RequestMetadata<'_>) -> RequestHints;
}
