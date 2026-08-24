//! Framework-neutral classification. Knows only HTTP.

use super::{FrameworkAdapter, RequestClass, RequestHints, RequestMetadata};
use http::Method;

pub struct Generic;

/// Classify using HTTP semantics alone. Every adapter falls back to this.
pub fn classify(req: &RequestMetadata<'_>) -> RequestClass {
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
