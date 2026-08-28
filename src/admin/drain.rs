//! Drain state: the window between "stop sending me work" and "stop serving".
//!
//! # Why a window is needed at all
//!
//! A load balancer learns an instance is unhealthy by *polling* it. If the
//! process starts refusing connections the instant it is signalled, every
//! request the balancer sent in the last poll interval is lost — and that is
//! the ordinary case, not the unlucky one. Draining inverts the order:
//! readiness starts failing while the process is still perfectly able to
//! serve, so the balancer withdraws it on its own schedule, and only then does
//! the shutdown begin.
//!
//! Harmost enters this state from three places:
//!
//! * `SIGTERM` / `SIGQUIT` / `SIGINT` — Pingora broadcasts its shutdown watch,
//!   and [`DrainWatcher`] turns that into drain state. This is the automatic
//!   path, and it is bounded by whatever Pingora's own grace period is.
//! * `SIGUSR1` — drain *without* exiting. This is the one a Kubernetes
//!   `preStop` hook or a rolling-restart script wants: send it, wait for the
//!   balancer to withdraw the instance, then send `SIGTERM`. Nothing else in
//!   the process reacts to `SIGUSR1`, and Pingora does not claim it.
//! * The explicit `drain_period` in [`crate::config::schema::Graceful`], which
//!   holds the process in drain state before the shutdown proper starts.
//!
//! Draining never affects liveness. A draining instance is alive and serving;
//! an orchestrator that killed it here would be undoing the whole mechanism.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use parking_lot::Mutex;
use pingora_core::server::ShutdownWatch;
use pingora_core::services::background::BackgroundService;
use tokio::signal::unix::{SignalKind, signal};

/// Shared, cheap to read from the request path and from the admin endpoints.
pub struct DrainState {
    draining: AtomicBool,
    /// When draining began, and why. Behind a lock because the pair has to be
    /// consistent; read only by the admin surface, never on the request path.
    since: Mutex<Option<(Instant, &'static str)>>,
}

impl Default for DrainState {
    fn default() -> Self {
        DrainState::new()
    }
}

impl DrainState {
    pub fn new() -> DrainState {
        DrainState {
            draining: AtomicBool::new(false),
            since: Mutex::new(None),
        }
    }

    /// Enter drain state. Idempotent: the *first* reason and instant are kept,
    /// so a `SIGUSR1` followed by a `SIGTERM` still reports how long the
    /// instance has actually been out of rotation.
    ///
    /// `reason` is `&'static str` rather than a `String` on purpose. It is
    /// rendered into the admin status document, and a closed set of literals
    /// chosen in this crate cannot carry anything a caller supplied.
    pub fn begin(&self, reason: &'static str) {
        let mut since = self.since.lock();
        if since.is_none() {
            *since = Some((Instant::now(), reason));
        }
        // Released after the timestamp is in place, so an observer that sees
        // `draining == true` always finds a `since` behind it.
        self.draining.store(true, Ordering::Release);
        crate::telemetry::metrics::DRAINING.set(1);
        log::warn!("draining: readiness now reports not-ready ({reason})");
    }

    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::Acquire)
    }

    pub fn draining_for(&self) -> Option<Duration> {
        self.since.lock().map(|(at, _)| at.elapsed())
    }

    pub fn reason(&self) -> &'static str {
        self.since.lock().map_or("-", |(_, why)| why)
    }
}

/// Turns Pingora's shutdown broadcast, and `SIGUSR1`, into drain state.
///
/// Runs as a background service so it shares the server runtime. It holds the
/// shutdown open for `drain_period` after the broadcast: Pingora's own grace
/// period governs in-flight *requests*, and this one governs the load
/// balancer's polling interval, which is a different and usually longer thing.
pub struct DrainWatcher {
    state: Arc<DrainState>,
    drain_period: Duration,
}

impl DrainWatcher {
    pub fn new(state: Arc<DrainState>, drain_period: Duration) -> DrainWatcher {
        DrainWatcher {
            state,
            drain_period,
        }
    }
}

#[async_trait]
impl BackgroundService for DrainWatcher {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        // `SIGUSR1` is unclaimed: Pingora takes SIGQUIT, SIGTERM and SIGINT,
        // and Harmost's reloader takes SIGHUP.
        let mut usr1 = match signal(SignalKind::user_defined1()) {
            Ok(s) => Some(s),
            Err(error) => {
                // Not fatal. The automatic drain on shutdown still works; only
                // the manual pre-stop trigger is unavailable.
                log::error!(
                    "could not install the SIGUSR1 handler; `drain on demand` is unavailable: {error}"
                );
                None
            }
        };
        loop {
            let usr1_recv = async {
                match usr1.as_mut() {
                    Some(s) => {
                        s.recv().await;
                    }
                    // No handler: never completes, so `select!` falls through
                    // to the shutdown arm.
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = shutdown.changed() => {
                    self.state.begin("shutdown");
                    if !self.drain_period.is_zero() {
                        log::info!(
                            "draining for {:?} before shutdown continues",
                            self.drain_period
                        );
                        // Keeps this service's task alive for the window, which
                        // is what Pingora waits on before the final step.
                        tokio::time::sleep(self.drain_period).await;
                    }
                    return;
                }
                () = usr1_recv => {
                    // Drain without exiting: the pre-stop hook's signal.
                    self.state.begin("sigusr1");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_state_is_not_draining() {
        let s = DrainState::new();
        assert!(!s.is_draining());
        assert!(s.draining_for().is_none());
        assert_eq!(s.reason(), "-");
    }

    #[test]
    fn beginning_a_drain_records_when_and_why() {
        let s = DrainState::new();
        s.begin("sigusr1");
        assert!(s.is_draining());
        assert_eq!(s.reason(), "sigusr1");
        assert!(s.draining_for().is_some());
    }

    #[test]
    fn a_second_drain_does_not_restart_the_clock() {
        // A pre-stop `SIGUSR1` followed by `SIGTERM` must still report how
        // long the instance has actually been out of rotation, or the
        // operator reading it concludes the drain window never elapsed.
        let s = DrainState::new();
        s.begin("sigusr1");
        std::thread::sleep(Duration::from_millis(20));
        s.begin("shutdown");
        assert_eq!(s.reason(), "sigusr1", "the first reason must be kept");
        assert!(
            s.draining_for().unwrap() >= Duration::from_millis(20),
            "the drain clock was reset"
        );
    }

    #[tokio::test]
    async fn the_watcher_drains_when_the_server_broadcasts_shutdown() {
        let state = Arc::new(DrainState::new());
        let watcher = DrainWatcher::new(state.clone(), Duration::ZERO);
        let (tx, rx) = tokio::sync::watch::channel(false);

        let task = tokio::spawn(async move { watcher.start(rx).await });
        assert!(!state.is_draining());
        tx.send(true).unwrap();
        task.await.unwrap();

        assert!(state.is_draining(), "shutdown did not enter drain state");
        assert_eq!(state.reason(), "shutdown");
    }

    #[tokio::test]
    async fn the_watcher_holds_the_drain_window_open() {
        // The window is the mechanism. A watcher that returned immediately
        // would drain and shut down in the same instant, which is the bug.
        let state = Arc::new(DrainState::new());
        let watcher = DrainWatcher::new(state.clone(), Duration::from_millis(200));
        let (tx, rx) = tokio::sync::watch::channel(false);
        let started = Instant::now();
        let task = tokio::spawn(async move { watcher.start(rx).await });
        tx.send(true).unwrap();
        task.await.unwrap();
        assert!(
            started.elapsed() >= Duration::from_millis(180),
            "the drain window was skipped: {:?}",
            started.elapsed()
        );
    }
}
