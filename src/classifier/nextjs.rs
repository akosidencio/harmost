//! Next.js adapter.
//!
//! Two things here are not configurable, because getting them wrong produces a
//! wrong response rather than a slow one:
//!
//! 1. **RSC is a variant of the same URL.** With `RSC: 1` a route returns a
//!    flight payload; without it, an HTML document. Same method, same path,
//!    same query. If those headers are not in the cache key, Harmost will
//!    eventually hand a flight payload to a browser as a document.
//! 2. **Draft mode must bypass.** `__prerender_bypass` and `__next_preview_data`
//!    put Next into draft mode, where the response contains unpublished
//!    content. Storing one and serving it publicly is a content leak.

use super::{FrameworkAdapter, RequestClass, RequestHints, RequestMetadata, generic};

/// Headers that select between different bodies at one URL. All four go into
/// the cache key whenever any of them is present.
pub const RSC_KEY_HEADERS: &[&str] = &[
    "rsc",
    "next-router-prefetch",
    "next-router-state-tree",
    "next-url",
];

/// Cookies that put Next into draft mode.
const DRAFT_COOKIES: &[&str] = &["__prerender_bypass", "__next_preview_data"];

pub struct NextJs;

impl NextJs {
    fn is_static_asset(path: &str) -> bool {
        path.starts_with("/_next/static/") || path == "/favicon.ico"
    }

    fn is_rsc(req: &RequestMetadata<'_>) -> bool {
        req.header("rsc").is_some()
    }

    fn is_prefetch(req: &RequestMetadata<'_>) -> bool {
        req.header("next-router-prefetch").is_some()
    }

    fn is_server_action(req: &RequestMetadata<'_>) -> bool {
        req.header("next-action").is_some()
    }

    fn in_draft_mode(req: &RequestMetadata<'_>) -> bool {
        DRAFT_COOKIES.iter().any(|c| req.has_cookie_named(c))
    }
}

impl FrameworkAdapter for NextJs {
    fn name(&self) -> &'static str {
        "nextjs"
    }

    fn classify_request(&self, req: &RequestMetadata<'_>) -> RequestHints {
        // Draft mode outranks everything, including a route that declares
        // itself public: the body contains unpublished content.
        if Self::in_draft_mode(req) {
            return RequestHints {
                class: Some(RequestClass::PrivateDynamic),
                force_bypass: Some("next_draft_mode"),
                ..Default::default()
            };
        }

        // A Server Action is a POST to the page's own URL. It must not be
        // confused with a document request for that same URL.
        if Self::is_server_action(req) {
            return RequestHints {
                class: Some(RequestClass::Mutation),
                force_bypass: Some("next_server_action"),
                ..Default::default()
            };
        }

        if Self::is_static_asset(req.path) {
            return RequestHints {
                class: Some(RequestClass::Static),
                ..Default::default()
            };
        }

        if Self::is_rsc(req) {
            let base = generic::classify(req);
            // A prefetch payload is keyed on the router state tree, which
            // encodes the whole client route state — near-unbounded
            // cardinality. Worth collapsing a burst of them, never worth
            // storing.
            let coalesce_only = Self::is_prefetch(req);
            return RequestHints {
                class: Some(base),
                key_headers: RSC_KEY_HEADERS.to_vec(),
                coalesce_only,
                force_bypass: None,
            };
        }

        RequestHints {
            class: Some(generic::classify(req)),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderMap, HeaderValue, Method, header};

    fn get<'a>(path: &'a str, h: &'a HeaderMap) -> RequestMetadata<'a> {
        RequestMetadata {
            method: &Method::GET,
            host: "shop.example.com",
            path,
            query: None,
            headers: h,
        }
    }

    #[test]
    fn next_static_assets_are_static() {
        let h = HeaderMap::new();
        let hints = NextJs.classify_request(&get("/_next/static/chunks/main-abc123.js", &h));
        assert_eq!(hints.class, Some(RequestClass::Static));
    }

    #[test]
    fn rsc_requests_put_the_variant_headers_in_the_key() {
        // Without this, the flight payload and the HTML document collide.
        let mut h = HeaderMap::new();
        h.insert("rsc", HeaderValue::from_static("1"));
        let hints = NextJs.classify_request(&get("/products/iphone", &h));
        assert!(hints.key_headers.contains(&"rsc"));
        assert!(hints.key_headers.contains(&"next-router-state-tree"));
    }

    #[test]
    fn a_plain_document_request_adds_no_variant_headers() {
        let h = HeaderMap::new();
        let hints = NextJs.classify_request(&get("/products/iphone", &h));
        assert!(hints.key_headers.is_empty());
        assert_eq!(hints.class, Some(RequestClass::PublicDocument));
    }

    #[test]
    fn prefetch_rsc_is_coalesced_but_never_stored() {
        let mut h = HeaderMap::new();
        h.insert("rsc", HeaderValue::from_static("1"));
        h.insert("next-router-prefetch", HeaderValue::from_static("1"));
        let hints = NextJs.classify_request(&get("/products/iphone", &h));
        assert!(
            hints.coalesce_only,
            "state-tree cardinality makes storing these pointless"
        );
    }

    #[test]
    fn server_actions_bypass_everything() {
        let mut h = HeaderMap::new();
        h.insert("next-action", HeaderValue::from_static("abc123"));
        let req = RequestMetadata {
            method: &Method::POST,
            host: "shop.example.com",
            path: "/checkout",
            query: None,
            headers: &h,
        };
        let hints = NextJs.classify_request(&req);
        assert_eq!(hints.class, Some(RequestClass::Mutation));
        assert_eq!(hints.force_bypass, Some("next_server_action"));
    }

    #[test]
    fn draft_mode_bypasses_even_on_a_public_looking_path() {
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            HeaderValue::from_static("foo=1; __prerender_bypass=xyz"),
        );
        let hints = NextJs.classify_request(&get("/blog/unpublished-post", &h));
        assert_eq!(hints.force_bypass, Some("next_draft_mode"));
        assert_eq!(hints.class, Some(RequestClass::PrivateDynamic));
    }

    #[test]
    fn draft_cookie_match_is_exact_not_substring() {
        // `not__prerender_bypass_x` must not trip the draft-mode check.
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            HeaderValue::from_static("not__prerender_bypass_x=1"),
        );
        let hints = NextJs.classify_request(&get("/blog/post", &h));
        assert_eq!(hints.force_bypass, None);
    }
}
