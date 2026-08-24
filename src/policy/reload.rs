//! Configuration reload on `SIGHUP`.
//!
//! Pingora already owns `SIGQUIT` (graceful upgrade), `SIGTERM` (graceful
//! terminate) and `SIGINT` (fast shutdown), so `SIGHUP` is the free one.
//!
//! Two rules make reload safe:
//!
//! 1. **A bad config is refused, and the running one is kept.** Reload happens
//!    when someone is changing something, which is often during an incident.
//!    Half-applying a config then is worse than not reloading at all.
//! 2. **Policy is swapped; stateful runtime is not.** Limiters are resized in
//!    place because their outstanding permits belong to them — swapping in a
//!    fresh semaphore would transiently double admitted concurrency, on
//!    precisely the change most likely to be made under load (raising a limit).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arc_swap::ArcSwap;
use async_trait::async_trait;
use pingora_core::server::ShutdownWatch;
use pingora_core::services::background::BackgroundService;
use tokio::signal::unix::{SignalKind, signal};

use super::PolicySnapshot;
use crate::admission::AdmissionController;

pub struct Reloader {
    path: String,
    policy: Arc<ArcSwap<PolicySnapshot>>,
    admission: Arc<AdmissionController>,
    generation: AtomicU64,
}

impl Reloader {
    pub fn new(
        path: impl Into<String>,
        policy: Arc<ArcSwap<PolicySnapshot>>,
        admission: Arc<AdmissionController>,
    ) -> Self {
        Reloader {
            path: path.into(),
            policy,
            admission,
            generation: AtomicU64::new(1),
        }
    }

    /// Re-read, validate, and apply. Returns the new generation, or an
    /// explanation of why the running config was kept.
    pub fn reload(&self) -> Result<u64, String> {
        let cfg = crate::config::load(&self.path).map_err(|e| {
            let mut msg = e.to_string();
            if let Some(src) = std::error::Error::source(&e) {
                msg.push_str(&format!(": {src}"));
            }
            msg
        })?;

        let current = self.policy.load();

        // Listener and upstream topology cannot change under a running
        // server. Pingora's SIGQUIT graceful upgrade is the tool for that,
        // and silently ignoring the edit would be worse than refusing it.
        if cfg.server.listen != current.config.server.listen {
            return Err(format!(
                "server.listen changed from {} to {}; that needs a restart \
                 (SIGQUIT performs a graceful upgrade)",
                current.config.server.listen, cfg.server.listen
            ));
        }
        if cfg.origin.upstreams != current.config.origin.upstreams {
            return Err(
                "origin.upstreams changed; that needs a restart (SIGQUIT performs a \
                 graceful upgrade)"
                    .to_string(),
            );
        }

        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;

        let route_limits: Vec<(String, usize, usize, std::time::Duration)> = cfg
            .routes
            .iter()
            .filter_map(|r| {
                r.concurrency.as_ref().map(|c| {
                    (r.id.clone(), c.max, c.queue.max, c.queue.timeout.as_duration())
                })
            })
            .collect();

        let snapshot = PolicySnapshot::build(cfg, generation).map_err(|e| e.to_string())?;

        // Resize before swapping: a request arriving on the new policy should
        // never find the old ceiling still in force.
        self.admission
            .apply_limits(snapshot.config.origin.concurrency.max, &route_limits);
        self.policy.store(snapshot);

        Ok(generation)
    }
}

#[async_trait]
impl BackgroundService for Reloader {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        let Ok(mut hup) = signal(SignalKind::hangup()) else {
            log::error!("could not install the SIGHUP handler; reload is unavailable");
            return;
        };
        loop {
            tokio::select! {
                _ = shutdown.changed() => return,
                _ = hup.recv() => match self.reload() {
                    Ok(generation) => {
                        log::info!("config reloaded from {} (generation {generation})", self.path);
                    }
                    Err(why) => {
                        // Keep serving the config that was already working.
                        log::error!("reload refused, keeping the running config: {why}");
                    }
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_config(body: &str) -> tempfile_lite::TempPath {
        let path = tempfile_lite::TempPath::new("harmost-reload");
        let mut f = std::fs::File::create(&path.0).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        path
    }

    const BASE: &str = r#"
version: 1
server:
  listen: "127.0.0.1:8080"
origin:
  upstreams: ["a:3000"]
  concurrency:
    max: 100
routes:
  - id: products
    match: "/products/**"
    concurrency:
      max: 10
"#;

    fn setup(body: &str) -> (tempfile_lite::TempPath, Reloader, Arc<ArcSwap<PolicySnapshot>>) {
        let path = write_config(body);
        let cfg = crate::config::load(&path.0).unwrap();
        let snapshot = PolicySnapshot::build(cfg, 1).unwrap();
        let admission = Arc::new(AdmissionController::new(100, 0, std::time::Duration::ZERO));
        let policy = Arc::new(ArcSwap::from(snapshot));
        let reloader = Reloader::new(path.0.clone(), policy.clone(), admission);
        (path, reloader, policy)
    }

    #[test]
    fn a_valid_change_is_applied_and_bumps_the_generation() {
        let (path, reloader, policy) = setup(BASE);
        std::fs::write(&path.0, BASE.replace("max: 10", "max: 42")).unwrap();

        let generation = reloader.reload().unwrap();
        assert_eq!(generation, 2);
        assert_eq!(policy.load().generation, 2);
        let route = &policy.load().routes[0];
        assert_eq!(route.config.concurrency.as_ref().unwrap().max, 42);
    }

    #[test]
    fn an_invalid_config_is_refused_and_the_running_one_kept() {
        let (path, reloader, policy) = setup(BASE);
        // private_dynamic plus a cache override is a leak; validation refuses it.
        std::fs::write(
            &path.0,
            format!("{BASE}    class: private_dynamic\n    cache:\n      override_origin: true\n      ttl:\n        max: 2s\n"),
        )
        .unwrap();

        assert!(reloader.reload().is_err());
        assert_eq!(policy.load().generation, 1, "a refused reload must not advance");
        assert_eq!(
            policy.load().routes[0].config.concurrency.as_ref().unwrap().max,
            10,
            "the running config must be untouched"
        );
    }

    #[test]
    fn unparseable_yaml_is_refused() {
        let (path, reloader, policy) = setup(BASE);
        std::fs::write(&path.0, "version: 1\n  this: is not: valid yaml\n").unwrap();
        let err = reloader.reload().unwrap_err();
        assert!(!err.is_empty());
        assert_eq!(policy.load().generation, 1);
    }

    #[test]
    fn changing_the_listener_is_refused_with_an_explanation() {
        let (path, reloader, _policy) = setup(BASE);
        std::fs::write(&path.0, BASE.replace("127.0.0.1:8080", "127.0.0.1:9999")).unwrap();
        let err = reloader.reload().unwrap_err();
        assert!(err.contains("needs a restart"), "{err}");
    }

    #[test]
    fn changing_upstreams_is_refused_with_an_explanation() {
        let (path, reloader, _policy) = setup(BASE);
        std::fs::write(&path.0, BASE.replace(r#"["a:3000"]"#, r#"["a:3000", "b:3000"]"#)).unwrap();
        let err = reloader.reload().unwrap_err();
        assert!(err.contains("needs a restart"), "{err}");
    }

    #[test]
    fn a_missing_file_is_refused() {
        let (path, reloader, policy) = setup(BASE);
        std::fs::remove_file(&path.0).unwrap();
        assert!(reloader.reload().is_err());
        assert_eq!(policy.load().generation, 1);
    }
}

/// Minimal temp-file helper. A whole crate for this in the dependency graph of
/// a proxy is not worth it.
#[cfg(test)]
mod tempfile_lite {
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    pub struct TempPath(pub String);

    impl TempPath {
        pub fn new(prefix: &str) -> TempPath {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            TempPath(format!("{}/{prefix}-{pid}-{n}.yaml", std::env::temp_dir().display()))
        }
    }

    impl Drop for TempPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}
