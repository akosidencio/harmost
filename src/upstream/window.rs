//! A ratio measured over a sliding window, without a lock or a sweep timer.
//!
//! Two things in this module's parent need the same shape: the circuit breaker
//! asks what fraction of recent origin requests failed, and the retry budget
//! asks what fraction of recent origin requests were retries. Both are a count
//! of events, a count of the subset that was *flagged*, and a window that
//! forgets.
//!
//! # Time is a parameter
//!
//! Every method takes `now_ms`, a millisecond count from a monotonic epoch the
//! caller owns. A window whose transitions are deadlines and which reads the
//! clock itself can only be tested by sleeping; passing time in makes a
//! ten-minute window exercisable in microseconds.
//!
//! # Accuracy under concurrency
//!
//! The window is a ring of buckets whose steady-state updates are lock-free.
//! Reusing one ring slot takes a short per-bucket lock so old counters are
//! cleared before the new epoch becomes visible. Two threads recording either
//! side of that rollover can still place one observation in the neighbouring
//! bucket; this feeds a threshold rather than a ledger, and that bounded
//! imprecision avoids a lock on every request.

use parking_lot::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

/// Slices the window is divided into.
///
/// Ten keeps the window from expiring all at once — the ratio decays as
/// buckets age out rather than dropping to zero on a boundary, which is what
/// makes a threshold flap.
const BUCKETS: usize = 10;

/// A millisecond count, saturating. `Duration::as_millis` is a `u128`, and a
/// silently narrowed window would be a different window.
pub fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

struct Bucket {
    /// Which window slice these counters belong to. A bucket whose epoch has
    /// aged past the window reads as zero rather than being swept by a timer.
    epoch: AtomicU64,
    total: AtomicU32,
    flagged: AtomicU32,
    /// Serialises reuse of this ring slot. Recording remains lock-free while a
    /// bucket belongs to the current epoch; the lock is taken only once per
    /// bucket rollover.
    rollover: Mutex<()>,
}

pub struct RollingRatio {
    bucket_ms: u64,
    buckets: Vec<Bucket>,
}

impl RollingRatio {
    pub fn new(window: Duration) -> Self {
        let window_ms = millis(window);
        RollingRatio {
            // At least 1ms: a sub-millisecond window would make every bucket
            // the same bucket and the ring pointless.
            bucket_ms: (window_ms / BUCKETS as u64).max(1),
            buckets: (0..BUCKETS)
                .map(|_| Bucket {
                    epoch: AtomicU64::new(0),
                    total: AtomicU32::new(0),
                    flagged: AtomicU32::new(0),
                    rollover: Mutex::new(()),
                })
                .collect(),
        }
    }

    /// Count one event, and whether it was of the flagged kind.
    pub fn record(&self, now_ms: u64, flagged: bool) {
        let Some(bucket) = self.current(now_ms) else {
            return;
        };
        bucket.total.fetch_add(1, Ordering::Relaxed);
        if flagged {
            bucket.flagged.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Flag an event that was already counted, returning the flagged total
    /// across the window *including* this one.
    ///
    /// Incrementing before deciding, rather than reading and then
    /// incrementing, is what keeps a burst of concurrent callers from all
    /// observing the same under-budget reading and all proceeding. A caller
    /// that does not like the answer gives it back with
    /// [`RollingRatio::unflag`].
    pub fn flag(&self, now_ms: u64) -> u64 {
        let Some(bucket) = self.current(now_ms) else {
            return u64::MAX;
        };
        bucket.flagged.fetch_add(1, Ordering::Relaxed);
        self.totals(now_ms).1
    }

    /// Give back a [`RollingRatio::flag`] that was not used.
    pub fn unflag(&self, now_ms: u64) {
        let Some(bucket) = self.current(now_ms) else {
            return;
        };
        // Saturating: a concurrent window rollover can clear the bucket
        // between the flag and its return, and a wrapped counter would read as
        // four billion flagged events.
        let _ = bucket
            .flagged
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Some(n.saturating_sub(1))
            });
    }

    /// `(total, flagged)` still inside the window.
    pub fn totals(&self, now_ms: u64) -> (u64, u64) {
        let epoch = now_ms / self.bucket_ms;
        let oldest = epoch.saturating_sub(BUCKETS as u64 - 1);
        let mut total = 0u64;
        let mut flagged = 0u64;
        for bucket in &self.buckets {
            let e = bucket.epoch.load(Ordering::Acquire);
            if e >= oldest && e <= epoch {
                total += u64::from(bucket.total.load(Ordering::Relaxed));
                flagged += u64::from(bucket.flagged.load(Ordering::Relaxed));
            }
        }
        (total, flagged)
    }

    /// Forget everything. Used when a state change makes the history
    /// misleading rather than merely old.
    pub fn reset(&self) {
        for bucket in &self.buckets {
            let _rollover = bucket.rollover.lock();
            // Make the old counters ineligible before clearing them. Epoch 0
            // is a real bucket near process startup, so publishing it first
            // would let a reader observe the old counters as current.
            bucket.epoch.store(u64::MAX, Ordering::Release);
            bucket.total.store(0, Ordering::Relaxed);
            bucket.flagged.store(0, Ordering::Relaxed);
            bucket.epoch.store(0, Ordering::Release);
        }
    }

    /// The bucket for `now_ms`, cleared first if it still holds an older slice.
    fn current(&self, now_ms: u64) -> Option<&Bucket> {
        let epoch = now_ms / self.bucket_ms;
        // The remainder is smaller than `BUCKETS`, so it always fits a
        // `usize`; the `ok()?` is unreachable rather than a fallback.
        let bucket = self
            .buckets
            .get(usize::try_from(epoch % BUCKETS as u64).ok()?)?;
        if bucket.epoch.load(Ordering::Acquire) != epoch {
            let _rollover = bucket.rollover.lock();
            if bucket.epoch.load(Ordering::Acquire) != epoch {
                // Clear before publishing the new epoch. Publishing first lets
                // `totals` attribute the entire old bucket to the new window,
                // and lets a concurrent writer add a new observation only for
                // this thread to erase it afterwards.
                bucket.total.store(0, Ordering::Relaxed);
                bucket.flagged.store(0, Ordering::Relaxed);
                bucket.epoch.store(epoch, Ordering::Release);
            }
        }
        Some(bucket)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window() -> RollingRatio {
        RollingRatio::new(Duration::from_millis(1000))
    }

    #[test]
    fn counts_totals_and_the_flagged_subset() {
        let w = window();
        for i in 0..10 {
            w.record(0, i % 2 == 0);
        }
        assert_eq!(w.totals(0), (10, 5));
    }

    #[test]
    fn observations_age_out_after_a_full_window() {
        let w = window();
        for _ in 0..10 {
            w.record(0, true);
        }
        assert_eq!(w.totals(0), (10, 10));
        assert_eq!(w.totals(2_000), (0, 0), "a full window later, forgotten");
    }

    #[test]
    fn the_window_slides_rather_than_expiring_all_at_once() {
        let w = window();
        // One event per 100ms bucket across the whole window.
        for slice in 0..10 {
            w.record(slice * 100, true);
        }
        assert_eq!(w.totals(900), (10, 10));
        // 500ms later, half the buckets have aged out and half have not.
        let (total, _) = w.totals(1_400);
        assert!(
            (4..=6).contains(&total),
            "the window dropped {total} of 10 at once"
        );
    }

    #[test]
    fn flag_returns_the_running_total_and_unflag_gives_it_back() {
        let w = window();
        w.record(0, false);
        assert_eq!(w.flag(0), 1);
        assert_eq!(w.flag(0), 2);
        w.unflag(0);
        assert_eq!(w.totals(0), (1, 1));
    }

    #[test]
    fn unflag_cannot_wrap_the_counter_below_zero() {
        let w = window();
        w.unflag(0);
        assert_eq!(w.totals(0), (0, 0));
    }

    #[test]
    fn reset_forgets_everything() {
        let w = window();
        for _ in 0..10 {
            w.record(0, true);
        }
        w.reset();
        assert_eq!(w.totals(0), (0, 0));
    }

    #[test]
    fn a_reused_bucket_is_cleared_before_its_new_epoch_is_visible() {
        let w = window();
        w.record(0, true);

        // Epoch 10 reuses epoch 0's ring slot. Once the new epoch is visible,
        // the old observation must already be gone.
        w.record(1_000, false);
        assert_eq!(w.totals(1_000), (1, 0));
    }
}
