//! Cache key construction.
//!
//! The key is *structural*, not hashed. The spec sketched `hash(host + path +
//! ...)`, which a disk or distributed store would need — but for an in-process
//! store, hashing only buys a shorter map key and costs collision safety. Two
//! distinct pages that collide under a 64-bit hash are one user's document
//! served to another; with structural equality that failure cannot happen at
//! all. A short fingerprint is derived separately, for logs only.

use crate::classifier::RequestMetadata;
use crate::config::schema::{QueryMode, QueryPolicy};
use std::hash::{DefaultHasher, Hash, Hasher};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    scheme: String,
    host: String,
    method: String,
    path: String,
    /// Canonical query: filtered by policy, then sorted so that `?a=1&b=2` and
    /// `?b=2&a=1` are one entry rather than two renders.
    query: String,
    /// Sorted `(name, value)` pairs for every header that selects a variant:
    /// configured `Vary`, the framework's own variant headers, and the
    /// negotiated content encoding.
    variant: Vec<(String, String)>,
    deployment: Option<String>,
}

impl CacheKey {
    /// Stable short id for logs and traces. Never used for lookup.
    pub fn fingerprint(&self) -> String {
        let mut h = DefaultHasher::new();
        self.hash(&mut h);
        format!("{:016x}", h.finish())
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// Render the key as one canonical string.
    ///
    /// Pingora's `CacheKey` takes a string primary and hashes it itself, so
    /// this is the bridge between our structural key and its storage key.
    /// Components are separated by a byte that cannot appear in any of them,
    /// so `host=a.com path=/b` and `host=a.com/b path=` cannot collide.
    pub fn canonical_string(&self) -> String {
        let mut out = String::with_capacity(128);
        for part in [
            self.scheme.as_str(),
            self.host.as_str(),
            self.method.as_str(),
            self.path.as_str(),
            self.query.as_str(),
            self.deployment.as_deref().unwrap_or(""),
        ] {
            out.push_str(part);
            out.push('\u{1f}');
        }
        for (name, value) in &self.variant {
            out.push_str(name);
            out.push('\u{1e}');
            out.push_str(value);
            out.push('\u{1f}');
        }
        out
    }
}

/// Everything that goes into a key, assembled by the caller from route policy
/// and adapter hints.
pub struct KeyBuilder<'a> {
    pub scheme: &'a str,
    pub query_policy: Option<&'a QueryPolicy>,
    /// Header names from route `cache.vary` plus the framework adapter's
    /// mandatory variant headers.
    pub variant_headers: &'a [&'a str],
    pub deployment: Option<&'a str>,
}

impl<'a> KeyBuilder<'a> {
    pub fn build(&self, req: &RequestMetadata<'_>) -> CacheKey {
        let mut variant: Vec<(String, String)> = self
            .variant_headers
            .iter()
            .filter_map(|name| {
                let lower = name.to_ascii_lowercase();
                req.header(&lower).map(|v| (lower, v.to_string()))
            })
            .collect();

        // Content coding is part of the identity of a stored body. Omit it and
        // a brotli entry eventually reaches a client that only reads gzip.
        variant.push(("~encoding".into(), normalize_accept_encoding(req).into()));
        variant.sort();

        CacheKey {
            scheme: self.scheme.to_ascii_lowercase(),
            host: normalize_host(req.host),
            method: req.method.as_str().to_ascii_uppercase(),
            path: req.path.to_string(),
            query: canonical_query(req.query.unwrap_or(""), self.query_policy),
            variant,
            deployment: self.deployment.map(str::to_string),
        }
    }
}

fn normalize_host(host: &str) -> String {
    let host = host.trim().to_ascii_lowercase();
    // Strip the default port so `example.com` and `example.com:80` are one key.
    host.strip_suffix(":80")
        .or_else(|| host.strip_suffix(":443"))
        .unwrap_or(&host)
        .to_string()
}

/// Collapse `Accept-Encoding` to the coding we would actually negotiate.
///
/// Bounded to three values on purpose: keying on the raw header would mint a
/// separate entry for every browser's exact ordering.
fn normalize_accept_encoding(req: &RequestMetadata<'_>) -> &'static str {
    let Some(raw) = req.header("accept-encoding") else {
        return "identity";
    };
    let raw = raw.to_ascii_lowercase();
    let accepts = |token: &str| {
        raw.split(',').any(|part| {
            let mut it = part.split(';');
            let name = it.next().unwrap_or("").trim();
            // `br;q=0` means "explicitly not br".
            let refused = it.any(|p| p.trim().replace(' ', "") == "q=0");
            name == token && !refused
        })
    };
    if accepts("br") {
        "br"
    } else if accepts("gzip") {
        "gzip"
    } else {
        "identity"
    }
}

/// Filter and sort the query string.
///
/// With no policy, every parameter is kept. That is correct — dropping a
/// parameter merges semantically different pages — but it also means a unique
/// parameter per request mints a unique key per request, and therefore a render
/// per request. An `include` allowlist is the fix, and admission control is
/// what bounds the damage until someone configures one.
fn canonical_query(raw: &str, policy: Option<&QueryPolicy>) -> String {
    if raw.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<(&str, &str)> = raw
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (k, v),
            None => (p, ""),
        })
        .filter(|(k, _)| match policy {
            None => true,
            Some(p) => {
                let listed = p.keys.iter().any(|want| want == k);
                match p.mode {
                    QueryMode::Include => listed,
                    QueryMode::Exclude => !listed,
                }
            }
        })
        .collect();
    pairs.sort_unstable();
    pairs
        .iter()
        .map(|(k, v)| if v.is_empty() { (*k).to_string() } else { format!("{k}={v}") })
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderMap, HeaderValue, Method};

    struct Ctx {
        headers: HeaderMap,
    }

    impl Ctx {
        fn new() -> Self {
            Ctx { headers: HeaderMap::new() }
        }
        fn with(mut self, k: &'static str, v: &'static str) -> Self {
            self.headers.insert(k, HeaderValue::from_static(v));
            self
        }
    }

    fn key(path: &str, query: Option<&str>, ctx: &Ctx, variant: &[&str], deployment: Option<&str>) -> CacheKey {
        let req = RequestMetadata {
            method: &Method::GET,
            host: "shop.example.com",
            path,
            query,
            headers: &ctx.headers,
        };
        KeyBuilder {
            scheme: "https",
            query_policy: None,
            variant_headers: variant,
            deployment,
        }
        .build(&req)
    }

    #[test]
    fn query_parameter_order_does_not_split_the_entry() {
        let c = Ctx::new();
        assert_eq!(
            key("/p", Some("a=1&b=2"), &c, &[], None),
            key("/p", Some("b=2&a=1"), &c, &[], None)
        );
    }

    #[test]
    fn different_paths_are_different_keys() {
        let c = Ctx::new();
        assert_ne!(key("/a", None, &c, &[], None), key("/b", None, &c, &[], None));
    }

    #[test]
    fn rsc_variant_separates_flight_payload_from_document() {
        // The bug this prevents: a browser navigating to /products/iphone
        // receiving the RSC flight payload instead of HTML.
        let doc = Ctx::new();
        let rsc = Ctx::new().with("rsc", "1");
        assert_ne!(
            key("/products/iphone", None, &doc, &["rsc"], None),
            key("/products/iphone", None, &rsc, &["rsc"], None)
        );
    }

    #[test]
    fn brotli_and_gzip_clients_get_separate_entries() {
        let br = Ctx::new().with("accept-encoding", "gzip, deflate, br");
        let gz = Ctx::new().with("accept-encoding", "gzip, deflate");
        assert_ne!(key("/p", None, &br, &[], None), key("/p", None, &gz, &[], None));
    }

    #[test]
    fn equivalent_encoding_preferences_share_one_entry() {
        let a = Ctx::new().with("accept-encoding", "br, gzip");
        let b = Ctx::new().with("accept-encoding", "gzip, br");
        assert_eq!(key("/p", None, &a, &[], None), key("/p", None, &b, &[], None));
    }

    #[test]
    fn explicitly_refused_encoding_is_not_selected() {
        let refuses_br = Ctx::new().with("accept-encoding", "br;q=0, gzip");
        let gz = Ctx::new().with("accept-encoding", "gzip");
        assert_eq!(key("/p", None, &refuses_br, &[], None), key("/p", None, &gz, &[], None));
    }

    #[test]
    fn deployment_id_prevents_reuse_across_builds() {
        let c = Ctx::new();
        assert_ne!(
            key("/p", None, &c, &[], Some("build-a")),
            key("/p", None, &c, &[], Some("build-b"))
        );
    }

    #[test]
    fn host_case_and_default_port_normalize() {
        let c = Ctx::new();
        let mut a = key("/p", None, &c, &[], None);
        a.host = normalize_host("SHOP.example.com:443");
        assert_eq!(a, key("/p", None, &c, &[], None));
    }

    #[test]
    fn include_policy_drops_unlisted_parameters() {
        let p = QueryPolicy { mode: QueryMode::Include, keys: vec!["q".into(), "page".into()] };
        // A cache-busting parameter must not mint a new key, or every request
        // becomes a render.
        assert_eq!(canonical_query("q=shoes&cachebust=91821", Some(&p)), "q=shoes");
        assert_eq!(canonical_query("q=shoes&page=2", Some(&p)), "page=2&q=shoes");
    }

    #[test]
    fn exclude_policy_drops_only_listed_parameters() {
        let p = QueryPolicy { mode: QueryMode::Exclude, keys: vec!["utm_source".into()] };
        assert_eq!(canonical_query("utm_source=fb&q=shoes", Some(&p)), "q=shoes");
    }

    #[test]
    fn no_policy_keeps_every_parameter() {
        assert_eq!(canonical_query("a=1&zz=2", None), "a=1&zz=2");
    }

    #[test]
    fn canonical_string_cannot_confuse_component_boundaries() {
        let c = Ctx::new();
        // Without a separator that cannot appear in a component, these two
        // would render to the same string.
        let a = key("/b", None, &c, &[], None);
        let mut b = key("", None, &c, &[], None);
        b.host = "shop.example.com/b".into();
        assert_ne!(a.canonical_string(), b.canonical_string());
    }

    #[test]
    fn canonical_string_matches_key_equality() {
        let c = Ctx::new();
        let a = key("/p", Some("a=1&b=2"), &c, &[], None);
        let b = key("/p", Some("b=2&a=1"), &c, &[], None);
        assert_eq!(a, b);
        assert_eq!(a.canonical_string(), b.canonical_string());
    }

    #[test]
    fn fingerprint_is_stable_and_distinguishing() {
        let c = Ctx::new();
        let a = key("/a", None, &c, &[], None);
        let b = key("/b", None, &c, &[], None);
        assert_eq!(a.fingerprint(), a.clone().fingerprint());
        assert_ne!(a.fingerprint(), b.fingerprint());
    }
}
