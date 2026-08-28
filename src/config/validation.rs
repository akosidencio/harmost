//! Startup refuses to run an unsafe configuration.
//!
//! Every check here corresponds to a way the config can be *syntactically*
//! valid while describing something that leaks data or defeats the point of
//! running Harmost at all. Failing at boot is the cheapest place to catch them.

use super::schema::*;
use std::collections::HashSet;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ValidationError(pub String);

pub type Result<T> = std::result::Result<T, ValidationError>;

fn err(msg: impl Into<String>) -> ValidationError {
    ValidationError(msg.into())
}

pub fn validate(cfg: &Config) -> Result<()> {
    if cfg.version != super::SCHEMA_VERSION {
        // Refused, never coerced. The format is pre-1.0 and a future version
        // may reinterpret a key that exists today, so a binary that guessed
        // would be applying a policy nobody wrote. Both numbers are in the
        // message because the useful next question is "which binary is this".
        return Err(err(format!(
            "config schema version {} is not supported; this build of harmost understands \
             version {}. See docs/CONFIG-SCHEMA.md for the compatibility rules and the \
             migration notes",
            cfg.version,
            super::SCHEMA_VERSION
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
    check_concurrency_max(cfg.origin.concurrency.max, "origin.concurrency.max")?;
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
    validate_server_tls(cfg)?;
    validate_trusted_proxies(cfg)?;
    validate_origin_tls(cfg)?;
    validate_spool(cfg)?;
    validate_upgrade(cfg)?;
    if let Some(prometheus) = &cfg.telemetry.prometheus {
        validate_listen(&prometheus.listen, "telemetry.prometheus.listen")?;
    }
    validate_graceful(cfg)?;
    validate_admin(cfg)?;
    validate_tracing(cfg)?;
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

/// TLS asked for and TLS delivered must be the same thing.
///
/// The failure this exists to prevent is a binary built without the `tls`
/// feature accepting a `server.tls` block and then serving nothing on that
/// address — an operator would see a valid config, a running process, and a
/// port that refuses connections, with no line anywhere saying why.
fn validate_server_tls(cfg: &Config) -> Result<()> {
    let Some(tls) = &cfg.server.tls else {
        return Ok(());
    };
    if !cfg!(feature = "tls") {
        return Err(err(
            "server.tls is set but this binary was built without the `tls` feature; rebuild with \
             `cargo build --features tls`, or remove server.tls and terminate TLS in front of \
             Harmost",
        ));
    }
    validate_listen(&tls.listen, "server.tls.listen")?;
    if tls.listen == cfg.server.listen {
        return Err(err(format!(
            "server.tls.listen and server.listen are both `{}`; one address cannot serve cleartext \
             and TLS at once",
            tls.listen
        )));
    }
    if tls.cert.is_empty() || tls.key.is_empty() {
        return Err(err("server.tls.cert and server.tls.key must both be set"));
    }
    // Checked here rather than at bind time so that `harmost check` catches a
    // missing certificate before a deploy rather than after one.
    for (path, label) in [(&tls.cert, "cert"), (&tls.key, "key")] {
        if !std::path::Path::new(path).exists() {
            return Err(err(format!("server.tls.{label} `{path}` does not exist")));
        }
    }
    #[cfg(feature = "tls")]
    validate_server_tls_material(&tls.cert, &tls.key)?;
    Ok(())
}

#[cfg(feature = "tls")]
fn validate_server_tls_material(cert_path: &str, key_path: &str) -> Result<()> {
    pingora_core::tls::install_default_crypto_provider();
    let material =
        pingora_core::tls::load_certs_and_key_files(cert_path, key_path).map_err(|error| {
            err(format!(
                "could not read server TLS certificate or key: {error}"
            ))
        })?;
    let Some((certs, key)) = material else {
        return Err(err(
            "server TLS files contain no usable certificate/private-key pair",
        ));
    };

    // Mirror Pingora's rustls listener construction here. Pingora 0.8 builds
    // this later through an infallible API that panics on malformed or
    // incompatible material; validation turns that startup panic into a
    // normal `harmost check` error.
    pingora_core::tls::ServerConfig::builder_with_protocol_versions(&[
        &pingora_core::tls::version::TLS12,
        &pingora_core::tls::version::TLS13,
    ])
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .map(|_| ())
    .map_err(|error| err(format!("server TLS certificate/key are invalid: {error}")))
}

/// A trust list that cannot be parsed is a trust list that does not protect
/// anything, and the symptom — forwarded headers silently ignored — looks
/// exactly like a working deployment until someone reads the logs.
fn validate_trusted_proxies(cfg: &Config) -> Result<()> {
    let proxies = &cfg.server.trusted_proxies;
    for block in &proxies.from {
        crate::net::forwarded::Cidr::parse(block)
            .map_err(|error| err(format!("server.trusted_proxies.from: {error}")))?;
    }
    // Naming a source without naming anyone to believe is a policy that reads
    // as "trust X-Forwarded-For" and behaves as "trust nothing".
    let reads_headers =
        proxies.client_ip != ForwardedSource::None || proxies.scheme != ForwardedSource::None;
    if proxies.from.is_empty()
        && reads_headers
        && cfg.server.trusted_proxies != TrustedProxies::default()
    {
        return Err(err(
            "server.trusted_proxies names a client_ip or scheme source but `from` is empty, so no \
             peer is ever trusted and neither is read; list the CIDR blocks your load balancer \
             connects from",
        ));
    }
    Ok(())
}

fn validate_origin_tls(cfg: &Config) -> Result<()> {
    let Some(tls) = &cfg.origin.tls else {
        // ALPN only exists inside a TLS handshake. Asking to negotiate over
        // cleartext is a request that cannot be honoured, and the honest
        // failure is here rather than as a connection error per request.
        if cfg.origin.http_version == OriginHttpVersion::Auto {
            return Err(err(
                "origin.http_version: auto negotiates over ALPN, which requires origin.tls; over \
                 cleartext choose http1 or http2 explicitly",
            ));
        }
        return Ok(());
    };
    if !cfg!(feature = "tls") {
        return Err(err(
            "origin.tls is set but this binary was built without the `tls` feature; rebuild with \
             `cargo build --features tls`",
        ));
    }
    if tls.sni.trim().is_empty() {
        return Err(err(
            "origin.tls.sni is empty; a peer with no SNI cannot be hostname-verified",
        ));
    }
    // Verifying the name against a chain nobody checked verifies nothing.
    if tls.verify_hostname && !tls.verify_cert {
        return Err(err(
            "origin.tls sets verify_hostname without verify_cert; a hostname is only meaningful once \
             the chain that vouches for it has been checked",
        ));
    }
    if tls.ca.is_some() {
        return Err(err(
            "origin.tls.ca is not implemented: Pingora 0.8's rustls connector does not read the \
             per-peer CA store (its connect path carries an explicit TODO and never calls \
             peer.get_ca()), so naming a CA here would verify against the system roots anyway. Add \
             the CA to the platform trust store, or point SSL_CERT_FILE / SSL_CERT_DIR at it — both \
             are honoured — or set verify_cert: false if the origin is reachable only over a private \
             network",
        ));
    }
    Ok(())
}

fn validate_spool(cfg: &Config) -> Result<()> {
    if cfg.spool.max_body.get() == 0 {
        return Err(err("spool.max_body must be greater than zero"));
    }
    if cfg.spool.max_memory.get() == 0 {
        return Err(err("spool.max_memory must be greater than zero"));
    }
    if cfg.spool.max_body > cfg.spool.max_memory {
        return Err(err(format!(
            "spool.max_body ({} bytes) exceeds spool.max_memory ({} bytes); \
             not one response could ever be spooled",
            cfg.spool.max_body.get(),
            cfg.spool.max_memory.get()
        )));
    }
    // Spooling a streaming route would hold every chunk until the last one,
    // which is the opposite of what the class is for. Refused rather than
    // ignored: silently not spooling is indistinguishable from spooling.
    for route in &cfg.routes {
        let asked = route.spool.as_ref().and_then(|spool| spool.enabled) == Some(true);
        if asked && route.class == Some(ClassOverride::Streaming) {
            return Err(err(format!(
                "route `{}` is class streaming and sets spool.enabled: true; a spool withholds the \
                 body until the origin finishes, which is exactly what a streaming route must not do",
                route.id
            )));
        }
    }
    Ok(())
}

fn validate_upgrade(cfg: &Config) -> Result<()> {
    check_concurrency_max(cfg.upgrade.max_concurrent, "upgrade.max_concurrent")?;
    if cfg.upgrade.enabled && cfg.upgrade.max_concurrent == 0 {
        return Err(err(
            "upgrade.enabled is true but upgrade.max_concurrent is 0, which admits nothing; set a \
             ceiling or disable upgrades",
        ));
    }
    Ok(())
}

/// The zero-downtime upgrade runs on these three paths agreeing between two
/// processes, so a nonsense value here is only discovered during the restart
/// it was supposed to make safe.
fn validate_graceful(cfg: &Config) -> Result<()> {
    let g = &cfg.server.graceful;
    if g.pid_file.trim().is_empty() {
        return Err(err(
            "server.graceful.pid_file is empty; omit the key to take the default",
        ));
    }
    if g.upgrade_socket.trim().is_empty() {
        return Err(err(
            "server.graceful.upgrade_socket is empty; omit the key to take the default",
        ));
    }
    if g.pid_file == g.upgrade_socket {
        return Err(err(
            "server.graceful.pid_file and server.graceful.upgrade_socket are the same path",
        ));
    }
    if g.shutdown_timeout == super::units::Dur::ZERO {
        return Err(err(
            "server.graceful.shutdown_timeout is 0, which cuts off every in-flight request \
             the moment a shutdown starts; that is the outage a graceful shutdown exists to \
             avoid",
        ));
    }
    Ok(())
}

/// The admin surface publishes backend health, cache occupancy and the config
/// generation. It must not share a listener with anything that serves the
/// public, and it must not be reachable at a path the origin also owns — which
/// is why it is an address rather than a route.
fn validate_admin(cfg: &Config) -> Result<()> {
    let Some(admin) = &cfg.telemetry.admin else {
        return Ok(());
    };
    validate_listen(&admin.listen, "telemetry.admin.listen")?;

    // Compared as parsed addresses, not as strings, so `127.0.0.1:9090` and
    // `127.000.000.001:9090` cannot slip past as "different".
    let admin_addr = admin.listen.parse::<std::net::SocketAddr>().ok();
    let mut taken: Vec<(&str, &str)> = vec![("server.listen", cfg.server.listen.as_str())];
    if let Some(tls) = &cfg.server.tls {
        taken.push(("server.tls.listen", tls.listen.as_str()));
    }
    if let Some(p) = &cfg.telemetry.prometheus {
        taken.push(("telemetry.prometheus.listen", p.listen.as_str()));
    }
    for (name, address) in taken {
        if address.parse::<std::net::SocketAddr>().ok() == admin_addr {
            return Err(err(format!(
                "telemetry.admin.listen `{}` is the same address as {name}; the admin \
                 endpoints publish backend health, cache occupancy and the configuration \
                 generation, and must not share a listener with traffic",
                admin.listen
            )));
        }
    }
    Ok(())
}

fn validate_tracing(cfg: &Config) -> Result<()> {
    let t = &cfg.telemetry.tracing;
    if t.service_name.as_ref().is_some_and(|n| n.trim().is_empty()) {
        return Err(err(
            "telemetry.tracing.service_name is empty; omit the key to take `harmost`",
        ));
    }
    if t.sample.one_in == 0 {
        // Ambiguous between "everything" and "nothing", and the two readings
        // differ by the entire contents of a tracing backend.
        return Err(err(
            "telemetry.tracing.sample.one_in is 0, which is ambiguous; use `one_in: 1` for \
             every request or `mode: never` for none",
        ));
    }
    let Some(otlp) = &t.otlp else {
        return Ok(());
    };
    crate::telemetry::otlp::parse_endpoint(&otlp.endpoint)
        .map_err(|why| err(format!("telemetry.tracing.otlp.endpoint: {why}")))?;
    if otlp.max_queue == 0 || otlp.max_batch == 0 {
        return Err(err(
            "telemetry.tracing.otlp.max_queue and max_batch must be greater than zero",
        ));
    }
    if otlp.max_batch > otlp.max_queue {
        return Err(err(format!(
            "telemetry.tracing.otlp.max_batch ({}) exceeds max_queue ({}); a batch can never \
             fill",
            otlp.max_batch, otlp.max_queue
        )));
    }
    if otlp.timeout == super::units::Dur::ZERO || otlp.interval == super::units::Dur::ZERO {
        return Err(err(
            "telemetry.tracing.otlp.timeout and interval must be greater than zero",
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

/// The longest a request may be made to wait for a permit.
///
/// A queue deadline is a wait on the request path, so an hour is already far
/// past anything an origin governor should allow; the bound exists to catch a
/// wrong unit (`timeout: 30m` where `30s` was meant) and to keep the value out
/// of the range where `Instant::now() + timeout` stops being representable.
const MAX_QUEUE_TIMEOUT: Duration = Duration::from_secs(60 * 60);

fn check_concurrency_max(value: usize, path: &str) -> Result<()> {
    if value > tokio::sync::Semaphore::MAX_PERMITS {
        return Err(err(format!(
            "{path} is {value}, greater than Tokio's semaphore maximum of {}",
            tokio::sync::Semaphore::MAX_PERMITS
        )));
    }
    Ok(())
}

fn check_queue(c: &Concurrency, path: &str) -> Result<()> {
    if c.queue.max > 0 && c.queue.timeout == super::units::Dur::ZERO {
        return Err(err(format!(
            "{path}.queue.max is {} but queue.timeout is 0; a queue with no deadline is unbounded in \
             time",
            c.queue.max
        )));
    }
    if c.queue.timeout.as_duration() > MAX_QUEUE_TIMEOUT {
        return Err(err(format!(
            "{path}.queue.timeout is {:?}, longer than the {:?} maximum; a request waiting \
             that long for a permit has already failed somewhere else",
            c.queue.timeout.as_duration(),
            MAX_QUEUE_TIMEOUT
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
        check_concurrency_max(c.max, &format!("route `{id}`.concurrency.max"))?;
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

    // ------------------------------------------------- schema versioning

    #[test]
    fn an_unknown_schema_version_is_refused_and_names_both_numbers() {
        // Coercing a future version would mean applying a policy nobody
        // wrote, because a later release may reinterpret a key that exists
        // today.
        let e = validate(&parse(BASE.replace("version: 1", "version: 2").as_str())).unwrap_err();
        assert!(e.0.contains('2') && e.0.contains("version 1"), "{e}");
        assert!(e.0.contains("CONFIG-SCHEMA"), "{e}");
        assert!(validate(&parse(BASE.replace("version: 1", "version: 0").as_str())).is_err());
    }

    // ------------------------------------------------- admin endpoints

    #[test]
    fn the_admin_listener_is_accepted_on_its_own_address() {
        validate(&parse(&format!(
            "{BASE}telemetry:\n  admin:\n    listen: \"127.0.0.1:9091\"\n"
        )))
        .unwrap();
    }

    #[test]
    fn the_admin_listener_may_not_share_an_address_with_traffic() {
        // It publishes backend health, cache occupancy and the config
        // generation. Sharing the traffic listener would publish all three to
        // anyone who can reach the site.
        let e = validate(&parse(&format!(
            "{BASE}server:\n  listen: \"127.0.0.1:8080\"\ntelemetry:\n  admin:\n    listen: \"127.0.0.1:8080\"\n"
        )))
        .unwrap_err();
        assert!(e.0.contains("server.listen"), "{e}");
    }

    #[test]
    fn the_admin_listener_may_not_share_an_address_with_metrics() {
        let e = validate(&parse(&format!(
            "{BASE}telemetry:\n  prometheus:\n    listen: \"127.0.0.1:9090\"\n  admin:\n    listen: \"127.0.0.1:9090\"\n"
        )))
        .unwrap_err();
        assert!(e.0.contains("telemetry.prometheus.listen"), "{e}");
    }

    #[test]
    fn a_malformed_admin_address_is_refused() {
        assert!(
            validate(&parse(&format!(
                "{BASE}telemetry:\n  admin:\n    listen: \"not-an-address\"\n"
            )))
            .is_err()
        );
    }

    // ------------------------------------------------- tracing

    #[test]
    fn a_tracing_block_without_export_is_accepted() {
        // Correlation is unconditional; export is the optional half.
        validate(&parse(&format!(
            "{BASE}telemetry:\n  tracing:\n    sample:\n      mode: ratio\n      one_in: 20\n"
        )))
        .unwrap();
    }

    #[test]
    fn an_https_otlp_endpoint_is_refused_rather_than_downgraded() {
        let e = validate(&parse(&format!(
            "{BASE}telemetry:\n  tracing:\n    otlp:\n      endpoint: \"https://collector:4318/v1/traces\"\n"
        )))
        .unwrap_err();
        assert!(e.0.contains("http://"), "{e}");
    }

    #[test]
    fn an_ambiguous_sample_ratio_is_refused() {
        // `one_in: 0` reads equally as "everything" and "nothing", and the two
        // differ by the entire contents of a tracing backend.
        let e = validate(&parse(&format!(
            "{BASE}telemetry:\n  tracing:\n    sample:\n      one_in: 0\n"
        )))
        .unwrap_err();
        assert!(e.0.contains("mode: never"), "{e}");
    }

    #[test]
    fn a_batch_larger_than_the_queue_is_refused() {
        let e = validate(&parse(&format!(
            "{BASE}telemetry:\n  tracing:\n    otlp:\n      endpoint: \"http://c:4318/v1/traces\"\n      max_queue: 10\n      max_batch: 100\n"
        )))
        .unwrap_err();
        assert!(e.0.contains("can never"), "{e}");
    }

    // ------------------------------------------------- graceful upgrade

    #[test]
    fn a_zero_shutdown_timeout_is_refused() {
        // It would cut off every in-flight request the instant a shutdown
        // begins, which is the outage graceful shutdown exists to avoid.
        let e = validate(&parse(&format!(
            "{BASE}server:\n  graceful:\n    shutdown_timeout: 0s\n"
        )))
        .unwrap_err();
        assert!(e.0.contains("shutdown_timeout"), "{e}");
    }

    #[test]
    fn the_pid_file_and_upgrade_socket_may_not_be_the_same_path() {
        let e = validate(&parse(&format!(
            "{BASE}server:\n  graceful:\n    pid_file: /run/h\n    upgrade_socket: /run/h\n"
        )))
        .unwrap_err();
        assert!(e.0.contains("same path"), "{e}");
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
    fn rejects_an_unparseable_trusted_proxy_block() {
        // A trust list that does not parse is a trust list that protects
        // nothing, and the symptom looks exactly like a working deployment.
        let cfg = parse(&format!(
            "{BASE}
server:
  trusted_proxies:
    from: [\"10.0.0.0/33\"]
"
        ));
        let e = validate(&cfg).unwrap_err().to_string();
        assert!(e.contains("trusted_proxies"), "{e}");
    }

    #[test]
    fn accepts_ipv4_ipv6_and_bare_addresses_in_the_trust_list() {
        let cfg = parse(&format!(
            "{BASE}
server:
  trusted_proxies:
    from: [\"10.0.0.0/8\", \"2001:db8::/32\", \"127.0.0.1\"]
    client_ip: forwarded
    scheme: forwarded
"
        ));
        validate(&cfg).unwrap();
    }

    #[test]
    fn rejects_a_forwarded_source_with_nobody_to_believe() {
        let cfg = parse(&format!(
            "{BASE}
server:
  trusted_proxies:
    client_ip: forwarded
    scheme: forwarded
"
        ));
        let e = validate(&cfg).unwrap_err().to_string();
        assert!(e.contains("`from` is empty"), "{e}");
    }

    #[test]
    fn the_default_trust_policy_is_accepted_and_believes_nobody() {
        // The default is not "unset and therefore an error": it is a real
        // policy — trust no peer — and it must validate.
        let cfg = parse(BASE);
        validate(&cfg).unwrap();
        assert!(cfg.server.trusted_proxies.from.is_empty());
    }

    #[test]
    fn rejects_alpn_negotiation_over_cleartext() {
        // There is no ALPN outside a TLS handshake, so `auto` over cleartext
        // is a request that cannot be honoured. Refused here rather than as a
        // connection error on every request.
        let cfg = parse(
            "version: 1
origin:
  upstreams: [\"next-1:3000\"]
  http_version: auto
",
        );
        let e = validate(&cfg).unwrap_err().to_string();
        assert!(e.contains("requires origin.tls"), "{e}");
    }

    #[test]
    fn accepts_prior_knowledge_h2c_to_the_origin() {
        let cfg = parse(
            "version: 1
origin:
  upstreams: [\"next-1:3000\"]
  http_version: http2
",
        );
        validate(&cfg).unwrap();
    }

    #[test]
    fn rejects_tls_when_the_binary_cannot_serve_it() {
        // The failure being prevented: a valid config, a running process, and
        // a TLS port that refuses connections with nothing in the logs.
        let cfg = parse(&format!(
            "{BASE}
server:
  tls:
    listen: \"0.0.0.0:8443\"
    cert: /nonexistent/fullchain.pem
    key: /nonexistent/privkey.pem
"
        ));
        let e = validate(&cfg).unwrap_err().to_string();
        if cfg!(feature = "tls") {
            assert!(e.contains("does not exist"), "{e}");
        } else {
            assert!(e.contains("`tls` feature"), "{e}");
        }
    }

    #[cfg(feature = "tls")]
    #[test]
    fn rejects_existing_but_invalid_server_tls_files() {
        let base = std::env::temp_dir().join(format!("harmost-invalid-tls-{}", std::process::id()));
        let cert = base.with_extension("cert.pem");
        let key = base.with_extension("key.pem");
        std::fs::write(&cert, b"this is not a certificate").unwrap();
        std::fs::write(&key, b"this is not a private key").unwrap();

        let cfg = parse(&format!(
            "{BASE}\nserver:\n  tls:\n    listen: \"0.0.0.0:8443\"\n    cert: {}\n    key: {}\n",
            cert.display(),
            key.display()
        ));
        let error = validate(&cfg).unwrap_err().to_string();

        let _ = std::fs::remove_file(cert);
        let _ = std::fs::remove_file(key);
        assert!(
            error.contains("no usable certificate/private-key pair")
                || error.contains("could not read server TLS"),
            "{error}"
        );
    }

    #[test]
    fn rejects_an_origin_ca_that_the_connector_would_ignore() {
        // The greengate rule: a key that is accepted and then ignored lets
        // someone ship believing a protection is on. Here the belief would be
        // "the origin's certificate is checked against my CA".
        let cfg = parse(
            "version: 1
origin:
  upstreams: [\"next-1:3000\"]
  tls:
    sni: origin.internal
    ca: /etc/harmost/ca.pem
",
        );
        let e = validate(&cfg).unwrap_err().to_string();
        if cfg!(feature = "tls") {
            assert!(e.contains("origin.tls.ca is not implemented"), "{e}");
        } else {
            assert!(e.contains("`tls` feature"), "{e}");
        }
    }

    #[test]
    fn rejects_hostname_verification_without_chain_verification() {
        let cfg = parse(
            "version: 1
origin:
  upstreams: [\"next-1:3000\"]
  tls:
    sni: origin.internal
    verify_cert: false
",
        );
        let e = validate(&cfg).unwrap_err().to_string();
        if cfg!(feature = "tls") {
            assert!(e.contains("verify_hostname without verify_cert"), "{e}");
        } else {
            assert!(e.contains("`tls` feature"), "{e}");
        }
    }

    #[test]
    fn rejects_a_spool_ceiling_that_can_never_be_met() {
        let cfg = parse(&format!(
            "{BASE}
spool:
  enabled: true
  max_body: 8MiB
  max_memory: 4MiB
"
        ));
        let e = validate(&cfg).unwrap_err().to_string();
        assert!(e.contains("exceeds spool.max_memory"), "{e}");
    }

    #[test]
    fn rejects_spooling_a_streaming_route() {
        // A spool withholds the body until the origin finishes, which is the
        // one thing a streaming route must not do. Refused rather than
        // ignored: silently not spooling is indistinguishable from spooling.
        let cfg = parse(&format!(
            "{BASE}
routes:
  - id: feed
    match: \"/feed\"
    class: streaming
    spool:
      enabled: true
"
        ));
        let e = validate(&cfg).unwrap_err().to_string();
        assert!(e.contains("class streaming"), "{e}");
    }

    #[test]
    fn rejects_an_upgrade_ceiling_that_admits_nothing() {
        let cfg = parse(&format!(
            "{BASE}
upgrade:
  enabled: true
  max_concurrent: 0
"
        ));
        let e = validate(&cfg).unwrap_err().to_string();
        assert!(e.contains("admits nothing"), "{e}");
    }

    #[test]
    fn rejects_concurrency_values_above_tokios_semaphore_maximum() {
        let too_many = tokio::sync::Semaphore::MAX_PERMITS + 1;
        for (yaml, path) in [
            (
                format!(
                    "version: 1\norigin:\n  upstreams: [\"next-1:3000\"]\n  concurrency:\n    max: {too_many}\n"
                ),
                "origin.concurrency.max",
            ),
            (
                format!("{BASE}upgrade:\n  max_concurrent: {too_many}\n"),
                "upgrade.max_concurrent",
            ),
            (
                format!(
                    "{BASE}routes:\n  - id: large\n    match: /large\n    concurrency:\n      max: {too_many}\n"
                ),
                "route `large`.concurrency.max",
            ),
        ] {
            let error = validate(&parse(&yaml)).unwrap_err().to_string();
            assert!(error.contains(path), "expected {path} in: {error}");
            assert!(error.contains("semaphore maximum"), "{error}");
        }
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
    fn rejects_a_queue_deadline_longer_than_the_maximum() {
        let cfg = parse(&format!(
            "{BASE}
routes:
  - id: r
    match: \"/x\"
    concurrency:
      max: 10
      queue:
        max: 100
        timeout: 2h
"
        ));
        assert!(
            validate(&cfg)
                .unwrap_err()
                .to_string()
                .contains("longer than the")
        );
    }

    /// The bound exists so that no accepted config can reach
    /// `Instant::now() + timeout` with a value that is not representable.
    #[test]
    fn the_longest_accepted_queue_deadline_is_a_representable_instant() {
        let cfg = parse(&format!(
            "{BASE}
routes:
  - id: r
    match: \"/x\"
    concurrency:
      max: 10
      queue:
        max: 100
        timeout: 60m
"
        ));
        validate(&cfg).expect("an hour is on the accepted side of the bound");
        assert!(
            tokio::time::Instant::now()
                .into_std()
                .checked_add(MAX_QUEUE_TIMEOUT)
                .is_some()
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
