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
    use harmost::proxy::Harmost;
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

    let listen = cfg.server.listen.clone();
    let concurrency = cfg.origin.concurrency.clone();
    let policy = match PolicySnapshot::build(cfg, 1) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

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

    let mut service = pingora_proxy::http_proxy_service(
        &server.configuration,
        Harmost::new(policy, admission),
    );
    service.add_tcp(&listen);
    server.add_service(service);

    eprintln!("harmost listening on {listen}");
    eprintln!("  origin concurrency ceiling: {}", concurrency.max);
    server.run_forever();
}

fn check(path: &str) -> ExitCode {
    match harmost::config::load(path) {
        Ok(cfg) => {
            let routes = cfg.routes.len();
            let upstreams = cfg.origin.upstreams.len();
            println!("ok: {path}");
            println!("  {upstreams} upstream(s), {routes} route(s)");
            println!("  global origin concurrency: {}", cfg.origin.concurrency.max);
            for r in &cfg.routes {
                let overrides = r.cache.as_ref().is_some_and(|c| c.override_origin);
                if overrides {
                    println!("  route `{}` overrides origin cache directives", r.id);
                }
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
