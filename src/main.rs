//! `harmost` — origin workload governor.

use std::process::ExitCode;

const USAGE: &str = "\
harmost — origin workload governor for server-rendered applications

USAGE:
    harmost check --config <FILE>   Validate a config file and exit
    harmost version                 Print the version and build features

    harmost run --config <FILE> [OPTIONS]

RUN OPTIONS:
    --upgrade    Take the listening sockets over from a running Harmost.
                 The old process keeps serving what it already accepted and
                 exits when those finish; no connection is refused in between.
                 Both processes must agree on server.graceful.upgrade_socket.
    --daemon     Fork into the background and write server.graceful.pid_file.
    --test       Bind everything, prove the process can start, and exit 0.
                 Run this before --upgrade: it turns \"the new binary cannot
                 start\" from an outage into a non-zero exit code.

SIGNALS:
    SIGHUP     reload the config; a bad one is refused and the running one kept
    SIGUSR1    start draining — readiness fails, traffic keeps being served
    SIGQUIT    graceful upgrade: hand the listeners to a process started with --upgrade
    SIGTERM    graceful shutdown
    SIGINT     fast shutdown
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str);

    match cmd {
        Some("version") => {
            // Features are part of the version because a binary that rejects
            // `server.tls` and one that terminates it are the same version
            // number otherwise, and "which build is this" is the first
            // question during an incident.
            println!(
                "harmost {} (config schema v{}, features: {})",
                env!("CARGO_PKG_VERSION"),
                harmost::config::SCHEMA_VERSION,
                if cfg!(feature = "tls") { "tls" } else { "none" }
            );
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
            Some(path) => {
                let flags = RunFlags {
                    upgrade: has_flag(&args, "--upgrade"),
                    daemon: has_flag(&args, "--daemon"),
                    test: has_flag(&args, "--test"),
                };
                if let Some(unknown) = unknown_run_flag(&args) {
                    eprintln!("harmost run: unknown option `{unknown}`\n\n{USAGE}");
                    return ExitCode::from(2);
                }
                run(&path, flags)
            }
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

/// What `run` was asked to do beyond starting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RunFlags {
    upgrade: bool,
    daemon: bool,
    test: bool,
}

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

/// An unrecognised `--flag`, if there is one.
///
/// The same rule the config file follows: an option that is accepted and then
/// ignored lets someone believe a process is daemonised, or upgrading, when it
/// is not. A typo has to be an error, not a silent default.
fn unknown_run_flag(args: &[String]) -> Option<&str> {
    let known = ["--upgrade", "--daemon", "--test", "--config", "-c", "run"];
    let mut skip_next = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--config" || arg == "-c" {
            skip_next = true;
            continue;
        }
        if arg.starts_with('-') && !known.contains(&arg.as_str()) {
            return Some(arg);
        }
    }
    None
}

fn run(path: &str, flags: RunFlags) -> ExitCode {
    use harmost::admin::Admin;
    use harmost::admin::drain::{DrainShutdownSignalWatch, DrainState, DrainWatcher};
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
    let admin_cfg = cfg.telemetry.admin.clone();
    let graceful = cfg.server.graceful.clone();
    let concurrency = cfg.origin.concurrency.clone();

    // Span export, if it is configured. Built before the server so a bad
    // endpoint is a startup failure rather than a background service that
    // logs once and never works — the same reason every other unusable
    // setting in this project is refused at boot.
    let tracing_cfg = cfg.telemetry.tracing.clone();
    let deployment_id = cfg.deployment.id.clone();
    let spans = match tracing_cfg.otlp.as_ref() {
        Some(otlp) => {
            let mut resource = vec![
                (
                    "service.name".to_string(),
                    tracing_cfg
                        .service_name
                        .clone()
                        .unwrap_or_else(|| "harmost".to_string()),
                ),
                (
                    "service.version".to_string(),
                    env!("CARGO_PKG_VERSION").to_string(),
                ),
            ];
            if let Some(id) = &deployment_id {
                resource.push(("deployment.id".to_string(), id.clone()));
            }
            match harmost::telemetry::otlp::build(otlp, resource) {
                Ok(pair) => Some(pair),
                Err(error) => {
                    eprintln!("error: telemetry.tracing.otlp: {error}");
                    return ExitCode::FAILURE;
                }
            }
        }
        None => None,
    };

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
    // Create every configured route limiter now rather than on the route's
    // first request. Reload already does this, and without it the admin
    // status document reports no route limits at all until traffic arrives —
    // so the first thing an operator checks after a deploy says the policy
    // they just shipped is not there.
    {
        let snapshot = policy.load();
        let route_limits: Vec<(String, usize, usize, std::time::Duration)> = snapshot
            .config
            .routes
            .iter()
            .filter_map(|r| {
                r.concurrency.as_ref().map(|c| {
                    (
                        r.id.clone(),
                        c.max,
                        c.queue.max,
                        c.queue.timeout.as_duration(),
                    )
                })
            })
            .collect();
        admission.apply_limits(
            concurrency.max,
            concurrency.queue.max,
            concurrency.queue.timeout.as_duration(),
            &route_limits,
        );
    }

    harmost::telemetry::metrics::CONFIG_GENERATION.set(1);
    harmost::telemetry::metrics::CONFIG_FINGERPRINT
        .set(i64::try_from(policy.load().fingerprint).unwrap_or(i64::MAX));
    // The ceilings the occupancy gauges are measured against. Published from
    // config rather than left for a dashboard to hardcode, so an alert cannot
    // go stale the first time somebody edits the budget.
    {
        let snapshot = policy.load();
        harmost::telemetry::metrics::CACHE_MAX_BYTES
            .set(i64::try_from(snapshot.config.cache.max_memory.get()).unwrap_or(i64::MAX));
        harmost::telemetry::metrics::SPOOL_MAX_BYTES
            .set(i64::try_from(snapshot.config.spool.max_memory.get()).unwrap_or(i64::MAX));
    }

    // Pingora's own server configuration, built from ours rather than from a
    // second file. The three fields that matter are the ones the zero-downtime
    // upgrade runs on: both processes have to agree on `upgrade_sock`, the pid
    // file is what every `kill -QUIT` in the documentation reads, and the two
    // grace periods bound how long the *old* process may keep serving after it
    // has handed its listeners over.
    let mut pingora_conf = pingora_core::server::configuration::ServerConf {
        pid_file: graceful.pid_file.clone(),
        upgrade_sock: graceful.upgrade_socket.clone(),
        daemon: flags.daemon,
        graceful_shutdown_timeout_seconds: Some(pingora_seconds(
            graceful.shutdown_timeout.as_duration(),
        )),
        ..Default::default()
    };
    // Harmost's signal watcher spends the load-balancer drain window before it
    // returns SIGTERM to Pingora. Once Pingora receives it, every listener
    // stops accepting immediately, so repeating the drain here would only add
    // a second silent wait after the useful window had already ended.
    pingora_conf.grace_period_seconds = Some(0);
    // Pingora's socket handover is Linux-only: on every other platform
    // `get_fds_from` logs "Upgrade is not currently supported" and returns
    // `ECONNREFUSED`, which reads exactly like "no old process is listening"
    // and sends an operator looking for a problem that is not there. Refuse
    // up front and name the real reason, and point at the drain-based restart
    // that does work everywhere.
    if flags.upgrade && !cfg!(target_os = "linux") {
        eprintln!(
            "error: --upgrade is not supported on this platform. Pingora can only pass \n\
             listening sockets between processes on Linux.\n\n\
             Use the drain-based restart instead: SIGUSR1 to this process, wait for your \n\
             load balancer to withdraw it (telemetry.admin /health/ready answers 503 \n\
             immediately), then SIGTERM and start the new process.\n\
             See docs/OPERATIONS.md."
        );
        return ExitCode::FAILURE;
    }

    let opt = pingora_core::server::configuration::Opt {
        upgrade: flags.upgrade,
        daemon: flags.daemon,
        test: flags.test,
        nocapture: false,
        conf: None,
    };
    let mut server = pingora_core::server::Server::new_with_opt_and_conf(opt, pingora_conf);
    // Under `--upgrade` this is where the listening sockets are taken over
    // from the running process, and under `--test` it exits zero once it has
    // proved the process can start.
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

    // Without an active checker, configured backends are assumed available.
    // With one, they remain unknown/unhealthy until a probe actually passes,
    // which makes `require_healthy_upstream` truthful during startup.
    if health_cfg.is_none() {
        upstreams.assume_healthy();
    }
    for backend in upstreams.backends() {
        harmost::telemetry::metrics::UPSTREAM_HEALTHY
            .with_label_values(&[&backend.address])
            .set(i64::from(upstreams.is_healthy(backend.id)));
    }

    server.add_service(pingora_core::services::background::background_service(
        "reload",
        Reloader::new(path.to_string(), policy.clone(), admission.clone()),
    ));
    eprintln!("  reload config with: kill -HUP <pid>");

    // Drain state is shared: the admin endpoints read it, the background
    // watcher sets it for SIGUSR1, and the server signal watcher sets it before
    // allowing SIGTERM to reach Pingora.
    let drain = Arc::new(DrainState::new());
    server.add_service(pingora_core::services::background::background_service(
        "drain",
        DrainWatcher::new(drain.clone()),
    ));
    eprintln!(
        "  drain without exiting with: kill -USR1 <pid>  (drain window {:?})",
        graceful.drain_period.as_duration()
    );

    let span_sink = match spans {
        Some((sink, exporter)) => {
            server.add_service(pingora_core::services::background::background_service(
                "otlp", exporter,
            ));
            Some(sink)
        }
        None => None,
    };

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

    let harmost = match Harmost::new(
        policy.clone(),
        admission.clone(),
        upstreams.clone(),
        span_sink,
    ) {
        Ok(harmost) => harmost,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    // Taken before the proxy is moved into its service. These are handles to
    // the same live state the request path uses, not copies: a status document
    // assembled from a startup snapshot would report zeros forever.
    let admin_listen = admin_cfg.as_ref().map(|a| a.listen.clone());
    let admin = admin_cfg.map(|admin_cfg| Admin {
        started: std::time::Instant::now(),
        config_path: path.to_string(),
        policy: policy.clone(),
        admission: admission.clone(),
        upstreams: upstreams.clone(),
        store: harmost.store(),
        spool: harmost.spool_budget(),
        upgrades: harmost.upgrade_limiter(),
        drain: drain.clone(),
        require_healthy_upstream: admin_cfg.require_healthy_upstream,
    });
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

    if let (Some(admin), Some(addr)) = (admin, admin_listen) {
        let mut service = pingora_core::services::listening::Service::new(
            "admin".to_string(),
            pingora_core::apps::http_app::HttpServer::new_app(admin),
        );
        service.add_tcp(&addr);
        server.add_service(service);
        eprintln!("harmost admin on {addr}  (/health/live, /health/ready, /status)");
    }

    eprintln!("harmost listening on {listen}");
    eprintln!("  origin concurrency ceiling: {}", concurrency.max);
    let run_args = pingora_core::server::RunArgs {
        shutdown_signal: Box::new(DrainShutdownSignalWatch::new(
            drain,
            graceful.drain_period.as_duration(),
        )),
    };
    server.run(run_args);
    ExitCode::SUCCESS
}

/// Pingora's lifecycle configuration is expressed in whole seconds. Round up
/// rather than truncate: `500ms` must never become an immediate shutdown, and
/// a configured safety window should be a floor rather than an accidental
/// shorter value.
fn pingora_seconds(duration: std::time::Duration) -> u64 {
    duration
        .as_secs()
        .saturating_add(u64::from(duration.subsec_nanos() != 0))
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
            println!(
                "  config schema v{} (harmost {})",
                cfg.version,
                env!("CARGO_PKG_VERSION")
            );
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
            // Operability surface, stated out loud for the same reason the
            // trust settings are: `check` is the last thing anyone reads
            // before deploying.
            match &cfg.telemetry.admin {
                Some(admin) => {
                    println!(
                        "  admin endpoints on {} (/health/live, /health/ready, /status)",
                        admin.listen
                    );
                    if admin.require_healthy_upstream {
                        println!(
                            "    readiness FAILS when no upstream is healthy; make sure something \
                             upstream of Harmost can route around this instance, or a degraded \
                             origin takes every replica out of rotation at once"
                        );
                    }
                    if admin
                        .listen
                        .parse::<std::net::SocketAddr>()
                        .is_ok_and(|a| a.ip().is_unspecified())
                    {
                        println!(
                            "    WARNING: bound to an unspecified address, so it is reachable on \
                             every interface. It publishes backend health, cache occupancy and \
                             the config generation; bind it to loopback or a private address"
                        );
                    }
                }
                None => println!(
                    "  no telemetry.admin: there is no readiness endpoint, so a load balancer \
                     cannot tell when this instance is draining"
                ),
            }
            match &cfg.telemetry.tracing.otlp {
                Some(otlp) => println!(
                    "  exporting spans to {} ({:?} batches of up to {})",
                    otlp.endpoint,
                    otlp.interval.as_duration(),
                    otlp.max_batch
                ),
                None => println!(
                    "  no telemetry.tracing.otlp: trace ids are still generated, logged and \
                     forwarded to the origin, but no spans leave this process"
                ),
            }
            if cfg.telemetry.tracing.trust_incoming
                == harmost::config::schema::TrustIncoming::FromTrustedProxies
            {
                println!(
                    "  WARNING: traceparent is trusted from server.trusted_proxies; those proxies \
                     must strip or replace client-supplied traceparent/tracestate headers, or an \
                     internet client can choose trace ids and force parent-based sampling"
                );
            }
            let drain_period = cfg.server.graceful.drain_period.as_duration();
            let shutdown_timeout = std::time::Duration::from_secs(pingora_seconds(
                cfg.server.graceful.shutdown_timeout.as_duration(),
            ));
            let stop_budget = drain_period.saturating_add(shutdown_timeout);
            println!(
                "  graceful restart: pid {}, socket {}",
                cfg.server.graceful.pid_file, cfg.server.graceful.upgrade_socket,
            );
            // Stated as one number because that is the number a supervisor is
            // configured with, and because `shutdown_timeout` is a floor
            // rather than a ceiling — a SIGTERM costs the full sum even on an
            // idle process. See the note on `Graceful` in the schema.
            println!(
                "    a SIGTERM takes about {stop_budget:?} ({drain_period:?} drain + \
                 {shutdown_timeout:?} shutdown), on an idle process too",
            );
            if stop_budget > std::time::Duration::from_secs(30) {
                println!(
                    "    WARNING: that exceeds Kubernetes' default \
                     terminationGracePeriodSeconds of 30, so the pod will be SIGKILLed \
                     part-way through the drain. Raise the supervisor's timeout to at \
                     least {}s or lower these two",
                    stop_budget.as_secs().saturating_add(5)
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
    fn pingora_timeouts_round_fractional_seconds_up() {
        assert_eq!(pingora_seconds(std::time::Duration::ZERO), 0);
        assert_eq!(pingora_seconds(std::time::Duration::from_millis(500)), 1);
        assert_eq!(pingora_seconds(std::time::Duration::from_secs(1)), 1);
        assert_eq!(pingora_seconds(std::time::Duration::from_millis(1500)), 2);
    }

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
