//! Framework-neutral classification. Knows only HTTP.

use super::{FrameworkAdapter, RequestClass, RequestHints, RequestMetadata};
use http::Method;

pub struct Generic;

/// Classify using HTTP semantics alone. Every adapter falls back to this.
pub fn classify(req: &RequestMetadata<'_>) -> RequestClass {
    // Before anything else, including the method check: an upgrade handshake
    // is a `GET` that carries cookies, and every rule below it would classify
    // one as an ordinary private document. It is neither — it is a request to
    // stop speaking HTTP, and it must not reach the cache or the render
    // permit under any other name.
    if req.is_upgrade() {
        return RequestClass::Upgrade;
    }

    if !req.is_safe_method() {
        return match *req.method {
            Method::POST | Method::PUT | Method::PATCH | Method::DELETE => RequestClass::Mutation,
            _ => RequestClass::Unknown,
        };
    }

    // A credential on the request means the response is about somebody.
    if req.has_authorization() {
        return RequestClass::PrivateDynamic;
    }

    // Cookies are the conservative default: assume personalised unless a route
    // says otherwise. Most of the internet's cache bugs live here.
    if req.has_cookies() {
        return RequestClass::PrivateDynamic;
    }

    if req.query.is_some_and(|q| !q.is_empty()) {
        return RequestClass::PublicDynamic;
    }

    RequestClass::PublicDocument
}

impl FrameworkAdapter for Generic {
    fn name(&self) -> &'static str {
        "generic"
    }

    fn classify_request(&self, req: &RequestMetadata<'_>) -> RequestHints {
        RequestHints {
            class: Some(classify(req)),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderMap, HeaderValue, Method, header};

    fn req<'a>(
        m: &'a Method,
        path: &'a str,
        q: Option<&'a str>,
        h: &'a HeaderMap,
    ) -> RequestMetadata<'a> {
        RequestMetadata {
            method: m,
            host: "example.com",
            path,
            query: q,
            headers: h,
        }
    }

    #[test]
    fn plain_get_is_a_public_document() {
        let h = HeaderMap::new();
        assert_eq!(
            classify(&req(&Method::GET, "/blog/post", None, &h)),
            RequestClass::PublicDocument
        );
    }

    #[test]
    fn query_string_demotes_to_public_dynamic() {
        let h = HeaderMap::new();
        assert_eq!(
            classify(&req(&Method::GET, "/search", Some("q=iphone"), &h)),
            RequestClass::PublicDynamic
        );
    }

    #[test]
    fn authorization_is_always_private() {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer x"));
        assert_eq!(
            classify(&req(&Method::GET, "/blog/post", None, &h)),
            RequestClass::PrivateDynamic
        );
    }

    #[test]
    fn cookies_are_private_by_default() {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, HeaderValue::from_static("session=abc"));
        assert_eq!(
            classify(&req(&Method::GET, "/", None, &h)),
            RequestClass::PrivateDynamic
        );
    }

    #[test]
    fn accept_header_cannot_bypass_admission() {
        let mut h = HeaderMap::new();
        h.insert(
            header::ACCEPT,
            HeaderValue::from_static("text/event-stream"),
        );
        assert_eq!(
            classify(&req(&Method::GET, "/feed", None, &h)),
            RequestClass::PublicDocument
        );
    }

    #[test]
    fn streaming_does_not_consume_an_origin_permit() {
        assert!(!RequestClass::Streaming.consumes_origin_permit());
        assert!(RequestClass::PublicDocument.consumes_origin_permit());
    }

    #[test]
    fn a_websocket_handshake_is_an_upgrade_not_a_document() {
        // The handshake is a plain GET. Without the upgrade check it lands in
        // `PublicDocument`, which is cacheable and coalescible — so two
        // sockets would be collapsed onto one and the `101` would be offered
        // to the microcache.
        let mut h = HeaderMap::new();
        h.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        h.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
        assert_eq!(
            classify(&req(&Method::GET, "/socket", None, &h)),
            RequestClass::Upgrade
        );
    }

    #[test]
    fn an_upgrade_outranks_the_cookie_and_query_rules() {
        // A real handshake carries the session cookie and often a query
        // string. Neither may reclassify it.
        let mut h = HeaderMap::new();
        h.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
        h.insert(header::COOKIE, HeaderValue::from_static("session=abc"));
        assert_eq!(
            classify(&req(&Method::GET, "/socket", Some("token=x"), &h)),
            RequestClass::Upgrade
        );
    }

    #[test]
    fn an_upgrade_is_never_shareable_and_never_holds_a_render_permit() {
        assert!(!RequestClass::Upgrade.storable_in_principle());
        assert!(!RequestClass::Upgrade.coalescible_in_principle());
        assert!(!RequestClass::Upgrade.consumes_origin_permit());
    }

    #[test]
    fn writes_are_mutations() {
        let h = HeaderMap::new();
        for m in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
            assert_eq!(
                classify(&req(&m, "/api/x", None, &h)),
                RequestClass::Mutation
            );
        }
    }
}
