//! Passive failure observation and per-backend circuit breaking.
//!
//! An active health check asks a backend one question, on one path, at one
//! interval. That is the point of it — it must stay cheap enough to run
//! forever — and it is also why it misses the failure modes that actually take
//! an SSR origin down. A Node process answering `/healthz` in a millisecond
//! while every render throws, a pod whose database pool is exhausted, a
//! container that lost its CPU share: all of them pass a probe and fail real
//! traffic. This module watches the real traffic instead, which costs no extra
//! requests because Harmost is already sending them.
//!
//! Like [`super::window`], every method takes `now_ms` rather than reading a
//! clock, so a state machine made entirely of minute-long deadlines is
//! testable in microseconds.

use std::sync::atomic::{AtomicU64, Ordering};

use super::window::{RollingRatio, millis};
use crate::config::schema::Breaker as BreakerConfig;

/// What the breaker decided about one backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    /// Taking traffic normally.
    Closed,
    /// Ejected. Only the routing fallbacks in [`super::UpstreamPool::select`]
    /// can still send it work.
    Open,
}

impl BreakerState {
    pub fn as_str(self) -> &'static str {
        match self {
            BreakerState::Closed => "closed",
            BreakerState::Open => "open",
        }
    }
}

pub struct Breaker {
    enabled: bool,
    min_requests: u64,
    failure_percent: u64,
    open_for_ms: u64,
    window: RollingRatio,
    /// Milliseconds until which this backend is ejected. `0` means closed.
    ///
    /// One atomic carries the whole state machine, which is what makes the
    /// half-open transition race-free: the thread that wins the
    /// compare-exchange past the deadline is the one probe, and it re-arms the
    /// deadline in the same operation. A probe whose result never arrives —
    /// the client hung up, the process is shutting down — therefore cannot
    /// leave the gate open, because the gate closed again the moment it was
    /// let through.
    open_until_ms: AtomicU64,
    /// How many times this backend has been ejected, for the status document.
    trips: AtomicU64,
}

impl Breaker {
    pub fn new(cfg: &BreakerConfig) -> Self {
        Breaker {
            enabled: cfg.enabled,
            min_requests: u64::from(cfg.min_requests),
            failure_percent: u64::from(cfg.failure_percent),
            open_for_ms: millis(cfg.open_for.as_duration()),
            window: RollingRatio::new(cfg.window.as_duration()),
            open_until_ms: AtomicU64::new(0),
            trips: AtomicU64::new(0),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn trips(&self) -> u64 {
        self.trips.load(Ordering::Relaxed)
    }

    /// Successes and failures still inside the window.
    pub fn counts(&self, now_ms: u64) -> (u64, u64) {
        let (total, failed) = self.window.totals(now_ms);
        (total - failed.min(total), failed)
    }

    /// Ejected right now, for reporting. Unlike [`Breaker::allows`] this never
    /// consumes the half-open probe, so a metrics scrape cannot spend the one
    /// request that was going to test recovery.
    pub fn state(&self) -> BreakerState {
        if self.open_until_ms.load(Ordering::Acquire) == 0 {
            BreakerState::Closed
        } else {
            BreakerState::Open
        }
    }

    /// May this backend be picked?
    ///
    /// Answering `true` past the open deadline is the half-open probe, and it
    /// re-arms the deadline as it goes, so exactly one request per `open_for`
    /// is spent finding out whether the backend came back.
    pub fn allows(&self, now_ms: u64) -> bool {
        if !self.enabled {
            return true;
        }
        loop {
            let until = self.open_until_ms.load(Ordering::Acquire);
            if until == 0 {
                return true;
            }
            if now_ms < until {
                return false;
            }
            match self.open_until_ms.compare_exchange_weak(
                until,
                self.deadline(now_ms),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                // This request is the probe.
                Ok(_) => return true,
                // Someone else took the probe, or closed the breaker outright.
                // Re-read rather than guessing which.
                Err(_) => continue,
            }
        }
    }

    /// Fold one origin outcome into the window.
    pub fn record(&self, now_ms: u64, ok: bool) {
        if !self.enabled {
            return;
        }
        if self.open_until_ms.load(Ordering::Acquire) != 0 {
            // This was the half-open probe: one result decides, because there
            // is by construction only one request in flight to decide it.
            if ok {
                // Start the recovery clean. Without this, the failures that
                // tripped the breaker are still inside the window, and the
                // first failure afterwards re-trips it immediately.
                self.window.reset();
                self.open_until_ms.store(0, Ordering::Release);
            } else {
                self.open_until_ms
                    .store(self.deadline(now_ms), Ordering::Release);
            }
            return;
        }

        self.window.record(now_ms, !ok);
        if !ok {
            self.trip_if_over_threshold(now_ms);
        }
    }

    /// A failure ratio can only rise on a failure, so this is called from
    /// exactly one place.
    fn trip_if_over_threshold(&self, now_ms: u64) {
        let (total, failed) = self.window.totals(now_ms);
        if total < self.min_requests {
            return;
        }
        // `failed / total >= percent / 100`, multiplied out. Both sides are
        // `u64` sums of `u32` counters, so neither product can overflow.
        if failed * 100 < total * self.failure_percent {
            return;
        }
        if self
            .open_until_ms
            .compare_exchange(
                0,
                self.deadline(now_ms),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            self.trips.fetch_add(1, Ordering::Relaxed);
            self.window.reset();
        }
    }

    /// `now_ms + open_for`, never zero — zero is the closed sentinel, and an
    /// `open_for` of nothing at time zero would read as "never tripped".
    fn deadline(&self, now_ms: u64) -> u64 {
        now_ms.saturating_add(self.open_for_ms).max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::units::Dur;
    use std::time::Duration;

    fn cfg(min_requests: u32, failure_percent: u32) -> BreakerConfig {
        BreakerConfig {
            enabled: true,
            window: Dur(Duration::from_millis(1000)),
            min_requests,
            failure_percent,
            open_for: Dur(Duration::from_millis(500)),
            max_ejected_percent: 50,
        }
    }

    #[test]
    fn a_disabled_breaker_never_ejects_anything() {
        let mut c = cfg(1, 1);
        c.enabled = false;
        let b = Breaker::new(&c);
        for _ in 0..100 {
            b.record(0, false);
        }
        assert!(b.allows(0));
        assert_eq!(b.state(), BreakerState::Closed);
    }

    #[test]
    fn a_handful_of_failures_is_noise_not_a_signal() {
        let b = Breaker::new(&cfg(20, 50));
        for _ in 0..19 {
            b.record(0, false);
        }
        assert!(b.allows(0), "tripped below min_requests");
        b.record(0, false);
        assert!(!b.allows(0), "20 failures out of 20 is not noise");
    }

    #[test]
    fn a_minority_of_failures_does_not_trip() {
        let b = Breaker::new(&cfg(10, 50));
        for i in 0..99 {
            b.record(0, i % 3 != 0); // two thirds succeed, a third fail
        }
        assert!(b.allows(0), "a third failing is under the 50% threshold");
        assert_eq!(b.counts(0), (66, 33));
    }

    #[test]
    fn a_majority_of_failures_trips() {
        let b = Breaker::new(&cfg(10, 50));
        // A third succeed. The ratio crosses 50% once `min_requests` is met.
        for i in 0..12 {
            b.record(0, i % 3 == 0);
        }
        assert!(!b.allows(0));
        assert_eq!(b.trips(), 1);
    }

    #[test]
    fn failures_age_out_of_the_window() {
        let b = Breaker::new(&cfg(10, 50));
        for _ in 0..9 {
            b.record(0, false);
        }
        // Two full windows later those failures are gone, so the next
        // failures have nothing to add to.
        let later = 2_000;
        for _ in 0..9 {
            b.record(later, true);
        }
        assert_eq!(b.counts(later), (9, 0));
        assert!(b.allows(later));
    }

    #[test]
    fn an_open_breaker_admits_exactly_one_probe_per_period() {
        let b = Breaker::new(&cfg(2, 50));
        b.record(0, false);
        b.record(0, false);
        assert!(!b.allows(0));
        assert!(!b.allows(499), "still inside open_for");

        assert!(b.allows(500), "the probe is admitted once open_for elapses");
        assert!(
            !b.allows(500),
            "a second request in the same instant must not also be a probe"
        );
        assert!(!b.allows(999), "the probe re-armed the deadline");
        assert!(b.allows(1000), "and the next period gets its own probe");
    }

    #[test]
    fn a_successful_probe_closes_the_breaker() {
        let b = Breaker::new(&cfg(2, 50));
        b.record(0, false);
        b.record(0, false);
        assert_eq!(b.state(), BreakerState::Open);

        assert!(b.allows(500));
        b.record(500, true);
        assert_eq!(b.state(), BreakerState::Closed);
        assert!(b.allows(500));
    }

    #[test]
    fn a_failed_probe_starts_the_period_again() {
        let b = Breaker::new(&cfg(2, 50));
        b.record(0, false);
        b.record(0, false);

        assert!(b.allows(500));
        b.record(500, false);
        assert_eq!(b.state(), BreakerState::Open);
        assert!(!b.allows(999));
        assert!(b.allows(1000));
    }

    /// The flap this guards against: the breaker closes, the failures that
    /// tripped it are still inside the window, and the very next failure
    /// pushes the ratio back over the threshold instantly.
    #[test]
    fn recovery_starts_with_a_clean_window() {
        let b = Breaker::new(&cfg(4, 50));
        for _ in 0..4 {
            b.record(0, false);
        }
        assert!(b.allows(500));
        b.record(500, true);
        assert_eq!(b.counts(500), (0, 0), "the old failures survived recovery");

        b.record(500, false);
        assert!(
            b.allows(500),
            "one failure after recovery re-tripped the breaker"
        );
    }

    #[test]
    fn trips_are_counted_for_the_status_document() {
        let b = Breaker::new(&cfg(2, 50));
        assert_eq!(b.trips(), 0);
        b.record(0, false);
        b.record(0, false);
        assert_eq!(b.trips(), 1);
        // Still open: further failures are the same trip, not new ones.
        b.record(10, false);
        assert_eq!(b.trips(), 1);
    }
}
