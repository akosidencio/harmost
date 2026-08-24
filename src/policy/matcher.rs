//! Route matching: host, path glob, method.

use crate::config::schema::{DetailedMatcher, Matcher};
use globset::{Glob, GlobMatcher};
use http::Method;

#[derive(Debug, thiserror::Error)]
#[error("route `{route}`: {message}")]
pub struct MatcherError {
    pub route: String,
    pub message: String,
}

/// `http::Method` parses any RFC 9110 token, so `GTE` round-trips happily as
/// an extension method. In a config file that is a typo, not an extension.
fn is_known_method(m: &Method) -> bool {
    matches!(
        *m,
        Method::GET
            | Method::HEAD
            | Method::POST
            | Method::PUT
            | Method::PATCH
            | Method::DELETE
            | Method::OPTIONS
            | Method::CONNECT
            | Method::TRACE
    )
}

pub struct CompiledMatcher {
    host: Option<String>,
    path: GlobMatcher,
    methods: Option<Vec<Method>>,
}

impl std::fmt::Debug for CompiledMatcher {
    /// Hand-written: deriving this dumps globset's entire compiled DFA into
    /// every assertion failure that touches a route.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledMatcher")
            .field("host", &self.host)
            .field("path", &self.path.glob().glob())
            .field("methods", &self.methods)
            .finish()
    }
}

impl CompiledMatcher {
    pub fn compile(route_id: &str, m: &Matcher) -> Result<Self, MatcherError> {
        let (host, path, methods) = match m {
            Matcher::Path(p) => (None, p.clone(), None),
            Matcher::Detailed(DetailedMatcher { host, path, methods }) => {
                (host.clone(), path.clone(), methods.clone())
            }
        };

        let glob = Glob::new(&path).map_err(|e| MatcherError {
            route: route_id.to_string(),
            message: format!("path pattern `{path}` is not a valid glob: {e}"),
        })?;

        let methods = methods
            .map(|list| {
                list.iter()
                    .map(|m| {
                        let parsed = m.parse::<Method>().ok().filter(is_known_method);
                        parsed.ok_or_else(|| MatcherError {
                            route: route_id.to_string(),
                            message: format!("`{m}` is not a valid HTTP method"),
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?;

        Ok(CompiledMatcher {
            host: host.map(|h| h.to_ascii_lowercase()),
            path: glob.compile_matcher(),
            methods,
        })
    }

    pub fn matches(&self, host: &str, path: &str, method: &Method) -> bool {
        if let Some(want) = &self.host
            && !host.eq_ignore_ascii_case(want)
        {
            return false;
        }
        if let Some(allowed) = &self.methods
            && !allowed.contains(method)
        {
            return false;
        }
        self.path.is_match(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::DetailedMatcher;

    fn path_matcher(p: &str) -> CompiledMatcher {
        CompiledMatcher::compile("t", &Matcher::Path(p.into())).unwrap()
    }

    #[test]
    fn double_star_matches_nested_paths() {
        let m = path_matcher("/products/**");
        assert!(m.matches("h", "/products/iphone", &Method::GET));
        assert!(m.matches("h", "/products/phones/iphone/15", &Method::GET));
        assert!(!m.matches("h", "/blog/post", &Method::GET));
    }

    #[test]
    fn exact_path_does_not_match_children() {
        let m = path_matcher("/search");
        assert!(m.matches("h", "/search", &Method::GET));
        assert!(!m.matches("h", "/search/advanced", &Method::GET));
    }

    #[test]
    fn host_must_match_when_specified() {
        let m = CompiledMatcher::compile(
            "t",
            &Matcher::Detailed(DetailedMatcher {
                host: Some("shop.example.com".into()),
                path: "/products/**".into(),
                methods: None,
            }),
        )
        .unwrap();
        assert!(m.matches("shop.example.com", "/products/x", &Method::GET));
        assert!(m.matches("SHOP.EXAMPLE.COM", "/products/x", &Method::GET));
        assert!(!m.matches("admin.example.com", "/products/x", &Method::GET));
    }

    #[test]
    fn method_list_is_enforced() {
        let m = CompiledMatcher::compile(
            "t",
            &Matcher::Detailed(DetailedMatcher {
                host: None,
                path: "/products/**".into(),
                methods: Some(vec!["GET".into(), "HEAD".into()]),
            }),
        )
        .unwrap();
        assert!(m.matches("h", "/products/x", &Method::GET));
        assert!(!m.matches("h", "/products/x", &Method::POST));
    }

    #[test]
    fn an_invalid_method_is_a_config_error() {
        let e = CompiledMatcher::compile(
            "bad",
            &Matcher::Detailed(DetailedMatcher {
                host: None,
                path: "/x".into(),
                methods: Some(vec!["GTE".into()]),
            }),
        )
        .unwrap_err();
        assert!(e.to_string().contains("not a valid HTTP method"));
    }
}
