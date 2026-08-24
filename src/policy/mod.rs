//! Immutable policy, resolved once per request.
//!
//! A snapshot is built at load, shared behind an `Arc`, and swapped wholesale
//! on reload. In-flight requests keep the generation they started with, so a
//! reload can never apply half of itself to one request.
//!
//! Note what a snapshot does *not* contain: limiters, flights, or cached
//! entries. Those are stateful and live in registries that outlive any one
//! generation — see [`crate::admission::AdmissionController`].

pub mod matcher;
pub mod reload;

use crate::classifier::RequestClass;
use crate::config::schema::{ClassOverride, Config, Route};
use http::Method;
use matcher::{CompiledMatcher, MatcherError};
use std::sync::Arc;

pub struct ResolvedRoute {
    pub id: String,
    pub matcher: CompiledMatcher,
    pub config: Route,
}

impl ResolvedRoute {
    /// A route's declared class, if it overrides what the classifier inferred.
    pub fn declared_class(&self) -> Option<RequestClass> {
        self.config.class.map(|c| match c {
            ClassOverride::Static => RequestClass::Static,
            ClassOverride::PublicSsr => RequestClass::PublicDocument,
            ClassOverride::PublicDynamic => RequestClass::PublicDynamic,
            ClassOverride::PrivateDynamic => RequestClass::PrivateDynamic,
            ClassOverride::Streaming => RequestClass::Streaming,
        })
    }
}

pub struct PolicySnapshot {
    pub routes: Vec<ResolvedRoute>,
    pub config: Config,
    pub generation: u64,
}

impl PolicySnapshot {
    pub fn build(config: Config, generation: u64) -> Result<Arc<Self>, MatcherError> {
        let routes = config
            .routes
            .iter()
            .map(|r| {
                Ok(ResolvedRoute {
                    id: r.id.clone(),
                    matcher: CompiledMatcher::compile(&r.id, &r.matcher)?,
                    config: r.clone(),
                })
            })
            .collect::<Result<Vec<_>, MatcherError>>()?;
        Ok(Arc::new(PolicySnapshot {
            routes,
            config,
            generation,
        }))
    }

    /// First match in file order wins.
    ///
    /// Predictability beats cleverness here: a "most specific pattern wins"
    /// rule makes the effect of adding a route depend on every other route,
    /// which is exactly the property you do not want when the thing being
    /// configured decides whether a response is shared.
    pub fn resolve(&self, host: &str, path: &str, method: &Method) -> Option<&ResolvedRoute> {
        self.routes
            .iter()
            .find(|r| r.matcher.matches(host, path, method))
    }

    /// The coalescing wait, resolved. Absent config tracks the origin timeout,
    /// because a waiter that gives up before its leader can finish creates the
    /// stampede it was supposed to prevent.
    pub fn coalesce_wait(&self) -> std::time::Duration {
        self.config
            .coalesce
            .wait_timeout
            .map(|d| d.as_duration())
            .unwrap_or_else(|| self.config.timeouts.origin.as_duration())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(yaml: &str) -> Arc<PolicySnapshot> {
        let cfg: Config = serde_saphyr::from_str(yaml).unwrap();
        crate::config::validation::validate(&cfg).unwrap();
        PolicySnapshot::build(cfg, 1).unwrap()
    }

    const YAML: &str = r#"
version: 1
origin:
  upstreams: ["next-1:3000"]
routes:
  - id: next-static
    match: "/_next/static/**"
  - id: products
    match: "/products/**"
    class: public_ssr
    cache:
      override_origin: true
      ttl:
        max: 2s
  - id: catchall
    match: "/**"
"#;

    #[test]
    fn first_matching_route_wins() {
        let s = snapshot(YAML);
        assert_eq!(
            s.resolve("h", "/products/iphone", &Method::GET).unwrap().id,
            "products"
        );
        assert_eq!(
            s.resolve("h", "/_next/static/a.js", &Method::GET)
                .unwrap()
                .id,
            "next-static"
        );
        assert_eq!(
            s.resolve("h", "/anything-else", &Method::GET).unwrap().id,
            "catchall"
        );
    }

    #[test]
    fn declared_class_maps_public_ssr_to_a_document() {
        let s = snapshot(YAML);
        let r = s.resolve("h", "/products/iphone", &Method::GET).unwrap();
        assert_eq!(r.declared_class(), Some(RequestClass::PublicDocument));
    }

    #[test]
    fn coalesce_wait_defaults_to_the_origin_timeout() {
        let s = snapshot(YAML);
        assert_eq!(s.coalesce_wait(), std::time::Duration::from_secs(30));
    }
}
