//! Startup refuses to run an unsafe configuration.
//!
//! Every check here corresponds to a way the config can be *syntactically*
//! valid while describing something that leaks data or defeats the point of
//! running Harmost at all. Failing at boot is the cheapest place to catch them.

use super::schema::*;
use std::collections::HashSet;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ValidationError(pub String);

pub type Result<T> = std::result::Result<T, ValidationError>;

fn err(msg: impl Into<String>) -> ValidationError {
    ValidationError(msg.into())
}

pub fn validate(cfg: &Config) -> Result<()> {
    if cfg.version != 1 {
        return Err(err(format!(
            "config version {} is not supported; this build understands version 1",
            cfg.version
        )));
    }
    if cfg.origin.upstreams.is_empty() {
        return Err(err(
            "origin.upstreams is empty; there is nothing to proxy to",
        ));
    }
    if cfg.origin.concurrency.max == 0 {
        return Err(err(
            "origin.concurrency.max is 0, which admits nothing; omit the key to take the default",
        ));
    }
    if cfg.cache.max_memory.get() == 0 {
        return Err(err("cache.max_memory must be greater than zero"));
    }
    if cfg.cache.max_body_size.get() == 0 {
        return Err(err("cache.max_body_size must be greater than zero"));
    }
    if cfg.cache.max_body_size > cfg.cache.max_memory {
        return Err(err(format!(
            "cache.max_body_size ({} bytes) exceeds cache.max_memory ({} bytes)",
            cfg.cache.max_body_size.get(),
            cfg.cache.max_memory.get()
        )));
    }
    if !(400..=599).contains(&cfg.overload.status) {
        return Err(err(format!(
            "overload.status {} is not a 4xx or 5xx; an overload response must be an error",
            cfg.overload.status
        )));
    }
    if cfg.deployment.id.is_some() && cfg.deployment.id_header.is_some() {
        return Err(err(
            "deployment.id and deployment.id_header are both set; pick one source of truth",
        ));
    }
    if cfg.deployment.id.as_ref().is_some_and(|id| {
        id.chars()
            .any(|c| matches!(c, '\u{1d}' | '\u{1e}' | '\u{1f}'))
    }) {
        return Err(err("deployment.id contains a reserved cache-key separator"));
    }

    validate_listen(&cfg.server.listen, "server.listen")?;
    if let Some(prometheus) = &cfg.telemetry.prometheus {
        validate_listen(&prometheus.listen, "telemetry.prometheus.listen")?;
    }
    for upstream in &cfg.origin.upstreams {
        validate_upstream(upstream)?;
    }
    if let Some(health) = &cfg.health {
        if !health.path.starts_with('/') {
            return Err(err("health.path must start with `/`"));
        }
        if health.interval == super::units::Dur::ZERO || health.timeout == super::units::Dur::ZERO {
            return Err(err(
                "health.interval and health.timeout must be greater than zero",
            ));
        }
        if health.healthy_after == 0 || health.unhealthy_after == 0 {
            return Err(err(
                "health.healthy_after and health.unhealthy_after must be greater than zero",
            ));
        }
    }

    check_unimplemented(cfg)?;
    check_coalesce_wait(cfg)?;
    check_queue(&cfg.origin.concurrency, "origin.concurrency")?;

    let mut seen: HashSet<&str> = HashSet::new();
    for route in &cfg.routes {
        if !seen.insert(route.id.as_str()) {
            return Err(err(format!("duplicate route id `{}`", route.id)));
        }
        check_route(route)?;
    }
    Ok(())
}

/// Reject options that parse but do nothing.
///
/// A config key that is accepted and then silently ignored is worse than one
/// that does not exist: it lets someone ship believing a protection is on. If
/// these get implemented, delete the corresponding check.
fn check_unimplemented(cfg: &Config) -> Result<()> {
    if !cfg.cache.respect_origin {
        return Err(err(
            "cache.respect_origin: false is not implemented; origin cache directives are \
             always honoured except where a route sets cache.override_origin",
        ));
    }
    if cfg.deployment.id_header.is_some() {
        return Err(err(
            "deployment.id_header is not implemented — a cache key is built before the \
             response exists, so the id cannot come from a response header; use \
             deployment.id instead",
        ));
    }
    Ok(())
}

fn validate_listen(address: &str, path: &str) -> Result<()> {
    address
        .parse::<std::net::SocketAddr>()
        .map(|_| ())
        .map_err(|_| {
            err(format!(
                "{path} `{address}` is not a valid IP socket address"
            ))
        })
}

fn validate_upstream(address: &str) -> Result<()> {
    let (host, port) = address
        .rsplit_once(':')
        .ok_or_else(|| err(format!("origin upstream `{address}` has no port")))?;
    if host.is_empty() || port.parse::<u16>().is_err() {
        return Err(err(format!(
            "origin upstream `{address}` is not a valid host:port"
        )));
    }
    if host.contains(':') && !(host.starts_with('[') && host.ends_with(']')) {
        return Err(err(format!(
            "origin upstream `{address}` uses an IPv6 address without brackets"
        )));
    }
    Ok(())
}

/// A waiter that gives up before the work it waits on can finish converts one
/// managed queue into a real stampede — the precise failure Harmost exists to
/// prevent. The wait must cover the origin timeout.
fn check_coalesce_wait(cfg: &Config) -> Result<()> {
    if let Some(wait) = cfg.coalesce.wait_timeout
        && wait < cfg.timeouts.origin
    {
        return Err(err(format!(
            "coalesce.wait_timeout ({:?}) is shorter than timeouts.origin ({:?}); \
             every waiter would be released to the origin before the leader could finish. \
             Omit wait_timeout to track the origin timeout automatically.",
            wait.as_duration(),
            cfg.timeouts.origin.as_duration()
        )));
    }
    Ok(())
}

fn check_queue(c: &Concurrency, path: &str) -> Result<()> {
    if c.queue.max > 0 && c.queue.timeout == super::units::Dur::ZERO {
        return Err(err(format!(
            "{path}.queue.max is {} but queue.timeout is 0; a queue with no deadline is unbounded in time",
            c.queue.max
        )));
    }
    Ok(())
}

fn check_route(route: &Route) -> Result<()> {
    let id = &route.id;
    let is_private = route.class == Some(ClassOverride::PrivateDynamic);

    if let Some(c) = &route.concurrency {
        if c.max == 0 {
            return Err(err(format!(
                "route `{id}`: concurrency.max is 0, which admits nothing"
            )));
        }
        check_queue(c, &format!("route `{id}`.concurrency"))?;
    }

    if let Some(cache) = &route.cache {
        if is_private && cache.enabled == Some(true) {
            return Err(err(format!(
                "route `{id}` is class private_dynamic but sets cache.enabled: true; \
                 per-user responses are never response-cached"
            )));
        }
        if cache.override_origin {
            if is_private {
                return Err(err(format!(
                    "route `{id}` is class private_dynamic and sets cache.override_origin; \
                     overriding the origin on a private route is how one user's page reaches another"
                )));
            }
            if route.class.is_none() {
                return Err(err(format!(
                    "route `{id}` sets cache.override_origin without declaring a class; \
                     add `class: public_ssr` to state that this route's responses are shareable"
                )));
            }
            if cache.ttl.as_ref().and_then(|t| t.max).is_none() {
                return Err(err(format!(
                    "route `{id}` sets cache.override_origin but no cache.ttl.max; \
                     an override with no ceiling has no bound on how stale a shared response can get"
                )));
            }
        }
        if let Some(v) = &cache.vary {
            for h in &v.headers {
                let lower = h.to_ascii_lowercase();
                if matches!(lower.as_str(), "cookie" | "authorization") {
                    return Err(err(format!(
                        "route `{id}`: cache.vary lists `{h}`; varying on a credential header \
                         creates one cache entry per user and is never what you want"
                    )));
                }
                if lower == "user-agent" || lower == "*" {
                    return Err(err(format!(
                        "route `{id}`: cache.vary lists `{h}`, which has effectively unbounded \
                         cardinality"
                    )));
                }
            }
        }
        if let Some(q) = &cache.query
            && q.keys.is_empty()
        {
            return Err(err(format!(
                "route `{id}`: cache.query.mode is set but cache.query.keys is empty"
            )));
        }
    }

    if let Some(co) = &route.coalesce
        && co.override_origin
        && is_private
    {
        return Err(err(format!(
            "route `{id}` is class private_dynamic and sets coalesce.override_origin; \
             per-user responses are never shared between requests"
        )));
    }
    if route
        .coalesce
        .as_ref()
        .is_some_and(|co| co.override_origin && co.enabled == Some(false))
    {
        return Err(err(format!(
            "route `{id}` sets coalesce.override_origin while coalescing is disabled"
        )));
    }

    if let Some(vary) = route.cache.as_ref().and_then(|cache| cache.vary.as_ref()) {
        let mut seen = HashSet::new();
        for name in &vary.headers {
            let parsed = http::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                err(format!(
                    "route `{id}`: cache.vary header `{name}` is invalid"
                ))
            })?;
            if !seen.insert(parsed) {
                return Err(err(format!(
                    "route `{id}`: cache.vary repeats header `{name}`"
                )));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config as _Config;

    fn parse(yaml: &str) -> _Config {
        serde_saphyr::from_str(yaml).expect("yaml should parse")
    }

    const BASE: &str = r#"
version: 1
origin:
  upstreams: ["next-1:3000"]
"#;

    #[test]
    fn accepts_a_minimal_config() {
        validate(&parse(BASE)).unwrap();
    }

    #[test]
    fn rejects_unknown_keys() {
        // A typo'd key is a silent policy change; serde must refuse it.
        let e = serde_saphyr::from_str::<_Config>(
            r#"
version: 1
origin:
  upstreams: ["a:3000"]
cache:
  enabled: true
  ttl_max: 2s
"#,
        )
        .unwrap_err();
        assert!(e.to_string().contains("unknown field"), "{e}");
    }

    #[test]
    fn rejects_coalesce_wait_shorter_than_origin_timeout() {
        let cfg = parse(&format!(
            "{BASE}
timeouts:
  origin: 30s
coalesce:
  wait_timeout: 2s
"
        ));
        let e = validate(&cfg).unwrap_err();
        assert!(
            e.to_string().contains("shorter than timeouts.origin"),
            "{e}"
        );
    }

    #[test]
    fn rejects_cache_override_on_private_route() {
        let cfg = parse(&format!(
            "{BASE}
routes:
  - id: account
    match: \"/account/**\"
    class: private_dynamic
    cache:
      override_origin: true
      ttl:
        max: 2s
"
        ));
        let e = validate(&cfg).unwrap_err();
        assert!(e.to_string().contains("private_dynamic"), "{e}");
    }

    #[test]
    fn rejects_cache_override_without_declared_class() {
        let cfg = parse(&format!(
            "{BASE}
routes:
  - id: products
    match: \"/products/**\"
    cache:
      override_origin: true
      ttl:
        max: 2s
"
        ));
        let e = validate(&cfg).unwrap_err();
        assert!(e.to_string().contains("without declaring a class"), "{e}");
    }

    #[test]
    fn rejects_cache_override_without_ttl_ceiling() {
        let cfg = parse(&format!(
            "{BASE}
routes:
  - id: products
    match: \"/products/**\"
    class: public_ssr
    cache:
      override_origin: true
"
        ));
        let e = validate(&cfg).unwrap_err();
        assert!(e.to_string().contains("no cache.ttl.max"), "{e}");
    }

    #[test]
    fn accepts_a_properly_fenced_override() {
        let cfg = parse(&format!(
            "{BASE}
routes:
  - id: products
    match: \"/products/**\"
    class: public_ssr
    cache:
      override_origin: true
      ttl:
        max: 2s
"
        ));
        validate(&cfg).unwrap();
    }

    #[test]
    fn rejects_varying_on_a_credential_header() {
        let cfg = parse(&format!(
            "{BASE}
routes:
  - id: r
    match: \"/x\"
    cache:
      vary:
        headers: [\"Cookie\"]
"
        ));
        assert!(
            validate(&cfg)
                .unwrap_err()
                .to_string()
                .contains("credential header")
        );
    }

    #[test]
    fn rejects_queue_without_deadline() {
        let cfg = parse(&format!(
            "{BASE}
routes:
  - id: r
    match: \"/x\"
    concurrency:
      max: 10
      queue:
        max: 100
        timeout: 0s
"
        ));
        assert!(
            validate(&cfg)
                .unwrap_err()
                .to_string()
                .contains("no deadline")
        );
    }

    #[test]
    fn rejects_config_that_would_silently_do_nothing() {
        // Each of these parses happily and has no effect, which is how someone
        // ships believing a protection is enabled.
        for (yaml, want) in [
            ("cache:\n  respect_origin: false\n", "respect_origin"),
            (
                "deployment:\n  id_header: \"X-Deployment-ID\"\n",
                "id_header",
            ),
        ] {
            let cfg = parse(&format!("{BASE}{yaml}"));
            let e = validate(&cfg).unwrap_err().to_string();
            assert!(e.contains(want), "expected {want} to be rejected, got: {e}");
            assert!(e.contains("not implemented"), "{e}");
        }
    }

    #[test]
    fn accepts_respect_origin_true_because_that_is_the_behaviour() {
        validate(&parse(&format!("{BASE}cache:\n  respect_origin: true\n"))).unwrap();
    }

    #[test]
    fn rejects_duplicate_route_ids() {
        let cfg = parse(&format!(
            "{BASE}
routes:
  - id: dup
    match: \"/a\"
  - id: dup
    match: \"/b\"
"
        ));
        assert!(
            validate(&cfg)
                .unwrap_err()
                .to_string()
                .contains("duplicate route id")
        );
    }

    #[test]
    fn rejects_a_body_limit_larger_than_the_cache_budget() {
        let cfg = parse(&format!(
            "{BASE}cache:\n  max_memory: 1KiB\n  max_body_size: 2KiB\n"
        ));
        assert!(
            validate(&cfg)
                .unwrap_err()
                .to_string()
                .contains("exceeds cache.max_memory")
        );
    }

    #[test]
    fn rejects_invalid_listeners_and_upstreams_before_pingora_can_panic() {
        let cfg = parse(
            "version: 1\nserver:\n  listen: nope\norigin:\n  upstreams: [\"missing-port\"]\n",
        );
        assert!(
            validate(&cfg)
                .unwrap_err()
                .to_string()
                .contains("server.listen")
        );

        let cfg = parse("version: 1\norigin:\n  upstreams: [\"missing-port\"]\n");
        assert!(
            validate(&cfg)
                .unwrap_err()
                .to_string()
                .contains("has no port")
        );
    }

    #[test]
    fn rejects_invalid_or_duplicate_vary_headers() {
        for headers in [
            "[\"bad header\"]",
            "[\"Accept-Language\", \"accept-language\"]",
        ] {
            let cfg = parse(&format!(
                "{BASE}routes:\n  - id: r\n    match: /x\n    cache:\n      vary:\n        headers: {headers}\n"
            ));
            assert!(validate(&cfg).is_err());
        }
    }

    #[test]
    fn accepts_requeue_now_that_lock_timeouts_have_an_implemented_path() {
        let cfg = parse(&format!(
            "{BASE}coalesce:\n  wait_timeout: 30s\n  on_timeout: requeue\n"
        ));
        validate(&cfg).unwrap();
    }
}
