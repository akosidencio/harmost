//! `harmost` — origin workload governor.

use std::process::ExitCode;

const USAGE: &str = "\
harmost — origin workload governor for server-rendered applications

USAGE:
    harmost check --config <FILE>   Validate a config file and exit
    harmost version                 Print the version

    harmost run --config <FILE>     Start the proxy
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str);

    match cmd {
        Some("version") => {
            println!("harmost {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("check") => match config_path(&args) {
            Some(path) => check(&path),
            None => {
                eprintln!("harmost check: --config <FILE> is required");
                ExitCode::from(2)
            }
        },
        Some("run") => match config_path(&args) {
            Some(path) => run(&path),
            None => {
                eprintln!("harmost run: --config <FILE> is required");
                ExitCode::from(2)
            }
        },
        Some("help") | Some("--help") | Some("-h") | None => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("harmost: unknown command `{other}`\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn config_path(args: &[String]) -> Option<String> {
    let i = args.iter().position(|a| a == "--config" || a == "-c")?;
    args.get(i + 1).cloned()
}

fn run(path: &str) -> ExitCode {
    use harmost::admission::AdmissionController;
    use harmost::policy::PolicySnapshot;
    use harmost::policy::reload::Reloader;
    use harmost::proxy::Harmost;
    use harmost::upstream::UpstreamPool;
    use harmost::upstream::health::HealthChecker;
    use std::sync::Arc;

    let cfg = match harmost::config::load(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            if let Some(s) = std::error::Error::source(&e) {
                eprintln!("  caused by: {s}");
            }
            return ExitCode::FAILURE;
        }
    };

    // Logs go to stderr at info by default; RUST_LOG overrides as usual.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();
    harmost::telemetry::metrics::preregister();

    let listen = cfg.server.listen.clone();
    let h2c = cfg.server.h2c;
    let tls = cfg.server.tls.clone();
    let prometheus_listen = cfg.telemetry.prometheus.as_ref().map(|p| p.listen.clone());
    let concurrency = cfg.origin.concurrency.clone();
    let policy = match PolicySnapshot::build(cfg, 1) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let policy = Arc::new(arc_swap::ArcSwap::from(policy));

    let admission = Arc::new(AdmissionController::new(
        concurrency.max,
        concurrency.queue.max,
        concurrency.queue.timeout.as_duration(),
    ));

    let mut server = match pingora_core::server::Server::new(None) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: could not start server: {e}");
            return ExitCode::FAILURE;
        }
    };
    server.bootstrap();

    // One pool, shared by the proxy and the health checker, so a probe result
    // is visible to routing immediately.
    let snapshot = policy.load();
    let upstreams = match UpstreamPool::new(
        &snapshot.config.origin.upstreams,
        snapshot.config.origin.load_balancing,
    ) {
        Ok(pool) => Arc::new(pool),
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    let health_cfg = snapshot.config.health.clone();
    drop(snapshot);

    server.add_service(pingora_core::services::background::background_service(
        "reload",
        Reloader::new(path.to_string(), policy.clone(), admission.clone()),
    ));
    eprintln!("  reload config with: kill -HUP <pid>");

    if let Some(health) = health_cfg {
        eprintln!(
            "  health checks: {} every {:?}",
            health.path,
            health.interval.as_duration()
        );
        server.add_service(pingora_core::services::background::background_service(
            "health",
            HealthChecker::new(upstreams.clone(), &health),
        ));
    }

    let harmost = match Harmost::new(policy, admission, upstreams) {
        Ok(harmost) => harmost,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut service = pingora_proxy::http_proxy_service(&server.configuration, harmost);

    // HTTP/2 over cleartext. Pingora peeks for the connection preface, so this
    // listener still serves HTTP/1.1 clients; it only decides whether an h2
    // preface is honoured or answered as a malformed HTTP/1.1 request.
    if h2c {
        // Refusing to start beats starting without the protocol that was
        // asked for: a listener that silently speaks only HTTP/1.1 is a
        // misconfiguration nobody notices until a client does.
        let Some(logic) = service.app_logic_mut() else {
            eprintln!("error: could not enable h2c: proxy service has no app logic");
            return ExitCode::FAILURE;
        };
        logic
            .server_options
            .get_or_insert_with(Default::default)
            .h2c = true;
        eprintln!("  h2c enabled on {listen}");
    }

    service.add_tcp(&listen);

    // Validation has already refused a `server.tls` block in a build without
    // the feature, so the `not(tls)` arm is unreachable in practice. It is
    // written out anyway: an unreachable branch that fails loudly costs
    // nothing, and the alternative — a `cfg` that silently drops the listener
    // — is the failure this whole arrangement exists to avoid.
    if let Some(tls) = &tls {
        #[cfg(feature = "tls")]
        match tls_settings(tls) {
            Ok(settings) => {
                service.add_tls_with_settings(&tls.listen, None, settings);
                eprintln!("harmost listening on {} (TLS)", tls.listen);
            }
            Err(error) => {
                eprintln!("error: server.tls: {error}");
                return ExitCode::FAILURE;
            }
        }
        #[cfg(not(feature = "tls"))]
        {
            eprintln!("error: server.tls: {}", tls_settings(tls).unwrap_err());
            return ExitCode::FAILURE;
        }
    }

    server.add_service(service);

    if let Some(addr) = &prometheus_listen {
        let mut metrics = pingora_core::services::listening::Service::prometheus_http_service();
        metrics.add_tcp(addr);
        server.add_service(metrics);
        eprintln!("harmost metrics on {addr}/metrics");
    }

    eprintln!("harmost listening on {listen}");
    eprintln!("  origin concurrency ceiling: {}", concurrency.max);
    server.run_forever();
}

/// Build the listener's TLS settings.
///
/// Two builds exist. With the `tls` feature this compiles against rustls and
/// terminates TLS in process; without it the function refuses, and validation
/// refuses earlier still. Neither build silently ignores a `server.tls` block:
/// a config that asks for TLS and gets cleartext is the worst possible outcome,
/// and the same reasoning is why every unimplemented key in this project is
/// rejected rather than skipped.
#[cfg(feature = "tls")]
fn tls_settings(
    tls: &harmost::config::schema::ServerTls,
) -> std::result::Result<pingora_core::listeners::tls::TlsSettings, String> {
    let mut settings = pingora_core::listeners::tls::TlsSettings::intermediate(&tls.cert, &tls.key)
        .map_err(|error| {
            format!(
                "could not load cert `{}`/key `{}`: {error}",
                tls.cert, tls.key
            )
        })?;
    if tls.h2 {
        // Offers `h2` alongside `http/1.1`, so an HTTP/1.1-only client is
        // never locked out of the TLS listener.
        settings.enable_h2();
    }
    Ok(settings)
}

#[cfg(not(feature = "tls"))]
fn tls_settings(
    _tls: &harmost::config::schema::ServerTls,
) -> std::result::Result<std::convert::Infallible, String> {
    Err(
        "this binary was built without the `tls` feature; rebuild with \
         `cargo build --features tls` or terminate TLS in front of Harmost"
            .to_string(),
    )
}

fn check(path: &str) -> ExitCode {
    match harmost::config::load(path) {
        Ok(cfg) => {
            let policy = match harmost::policy::PolicySnapshot::build(cfg, 1) {
                Ok(policy) => policy,
                Err(e) => {
                    eprintln!("error: invalid configuration in {path}");
                    eprintln!("  caused by: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let cfg = &policy.config;
            let routes = cfg.routes.len();
            let upstreams = cfg.origin.upstreams.len();
            println!("ok: {path}");
            println!("  {upstreams} upstream(s), {routes} route(s)");
            println!(
                "  global origin concurrency: {}",
                cfg.origin.concurrency.max
            );
            for r in &cfg.routes {
                let overrides = r.cache.as_ref().is_some_and(|c| c.override_origin);
                if overrides {
                    println!("  route `{}` overrides origin cache directives", r.id);
                }
            }
            // Everything below trades a safety property for convenience.
            // `check` is the last place someone reads before deploying, so it
            // says so out loud rather than leaving it in the file.
            if cfg.server.trusted_proxies.from.is_empty() {
                println!(
                    "  no server.trusted_proxies: forwarded client addresses and schemes \
                     are ignored, and the connection peer is treated as the client"
                );
            } else {
                println!(
                    "  trusting forwarded headers from {} block(s)",
                    cfg.server.trusted_proxies.from.len()
                );
            }
            if let Some(tls) = &cfg.origin.tls
                && (!tls.verify_cert || !tls.verify_hostname)
            {
                println!(
                    "  WARNING: origin.tls does not verify the origin's certificate; \
                     the connection is encrypted but not authenticated"
                );
            }
            if cfg.upgrade.enabled {
                println!(
                    "  Upgrade/WebSocket proxying enabled, up to {} concurrent connections",
                    cfg.upgrade.max_concurrent
                );
            }
            if cfg.spool.enabled
                || cfg
                    .routes
                    .iter()
                    .any(|r| r.spool.as_ref().and_then(|spool| spool.enabled) == Some(true))
            {
                println!(
                    "  response spooling enabled: up to {} bytes per response, {} bytes overall; \
                     spooled routes lose progressive rendering",
                    cfg.spool.max_body.get(),
                    cfg.spool.max_memory.get()
                );
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            let mut src = std::error::Error::source(&e);
            while let Some(s) = src {
                eprintln!("  caused by: {s}");
                src = s.source();
            }
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_rejects_a_matcher_that_cannot_be_compiled() {
        let path = std::env::temp_dir().join(format!(
            "harmost-check-{}-{}.yaml",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::write(
            &path,
            "version: 1\norigin:\n  upstreams: [\"127.0.0.1:3000\"]\nroutes:\n  - id: bad\n    match: \"[\"\n",
        )
        .unwrap();
        let result = check(path.to_str().unwrap());
        let _ = std::fs::remove_file(path);
        assert_eq!(result, ExitCode::FAILURE);
    }
}
