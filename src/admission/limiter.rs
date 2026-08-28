//! A concurrency limiter whose ceiling can move while requests are in flight.
//!
//! Reload is why this exists. Policy is immutable and swapped wholesale, but a
//! limiter is *stateful*: its outstanding permits belong to it. Swapping in a
//! fresh `Semaphore` on reload would let the old permits and the new ones both
//! be valid at once, transiently doubling admitted concurrency — on precisely
//! the config change most likely to be made during an incident, which is
//! raising a limit.

use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

/// A timeout in whole milliseconds, saturating.
///
/// `Duration::as_millis` is a `u128`. Clamping keeps an absurd configured
/// timeout absurd instead of wrapping it into a small one, and keeps the value
/// below the range where `Instant::now() + timeout` would overflow.
fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Why a request was not admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShedReason {
    /// The queue was already at its bound. Refused immediately.
    QueueFull,
    /// Waited in the queue and the deadline passed.
    QueueTimeout,
}

impl ShedReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ShedReason::QueueFull => "queue_full",
            ShedReason::QueueTimeout => "queue_timeout",
        }
    }
}

/// One unit of origin work in flight. Releasing it is what lets the next
/// request in, so it is held for exactly as long as the origin is busy.
#[derive(Debug)]
pub struct Permit {
    inner: Option<OwnedSemaphorePermit>,
    limiter: Arc<Limiter>,
    /// A second permit released at the same moment as this one. A request
    /// needs both route and global capacity, and they must be given back
    /// together or one limiter drifts out of step with the other.
    companion: Option<Box<Permit>>,
}

impl Permit {
    pub fn with_companion(mut self, other: Option<Permit>) -> Permit {
        self.companion = other.map(Box::new);
        self
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        if let Some(permit) = self.inner.take() {
            // Resize and return form one semaphore transaction. Without this
            // guard, a shrink can forget the currently available permits, a
            // group of in-flight permits can return, and only then can the
            // shrink record its debt. Queued requests can steal every permit
            // returned in that window and keep concurrency near the old
            // ceiling for another generation of work.
            let _resize = self.limiter.resize_lock.lock();
            if self.limiter.claim_debt() {
                // Absorb this returned permit directly. Returning it to the
                // semaphore first lets an already-queued waiter steal it.
                permit.forget();
            } else {
                // Make the permit visible while the guard is still held. A
                // following resize therefore observes and collects it.
                drop(permit);
            }
        }
    }
}

#[derive(Debug)]
pub struct Limiter {
    name: String,
    sem: Arc<Semaphore>,
    /// The configured ceiling, which is not the same as the semaphore's
    /// available count.
    limit: AtomicUsize,
    /// Permits a shrink still owes but could not take, because they were
    /// out with in-flight requests at the time.
    debt: AtomicUsize,
    /// Serialises ceiling changes with permit returns. The critical sections
    /// contain no await and only semaphore/atomic operations.
    resize_lock: Mutex<()>,
    queue_depth: AtomicUsize,
    queue_max: AtomicUsize,
    /// Milliseconds, not a `Duration`, because it has to live in an atomic.
    /// `u64` rather than `usize`: `Duration::as_millis` is a `u128`, and
    /// narrowing it to a pointer-sized integer silently rewrites a large
    /// configured timeout into a different one on a 32-bit target.
    queue_timeout_ms: AtomicU64,

    pub admitted: AtomicUsize,
    pub queued: AtomicUsize,
    pub shed: AtomicUsize,
}

impl Limiter {
    pub fn new(
        name: impl Into<String>,
        limit: usize,
        queue_max: usize,
        queue_timeout: Duration,
    ) -> Arc<Self> {
        Arc::new(Limiter {
            name: name.into(),
            sem: Arc::new(Semaphore::new(limit)),
            limit: AtomicUsize::new(limit),
            debt: AtomicUsize::new(0),
            resize_lock: Mutex::new(()),
            queue_depth: AtomicUsize::new(0),
            queue_max: AtomicUsize::new(queue_max),
            queue_timeout_ms: AtomicU64::new(millis(queue_timeout)),
            admitted: AtomicUsize::new(0),
            queued: AtomicUsize::new(0),
            shed: AtomicUsize::new(0),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn limit(&self) -> usize {
        self.limit.load(Ordering::Relaxed)
    }

    pub fn queue_depth(&self) -> usize {
        self.queue_depth.load(Ordering::Relaxed)
    }

    pub fn available(&self) -> usize {
        self.sem.available_permits()
    }

    /// Move the ceiling without disturbing in-flight work.
    ///
    /// Resizes are serialised with each other and with permit returns so a
    /// downward change has no interval in which returned permits can escape
    /// its debt accounting.
    pub fn resize(self: &Arc<Self>, new_limit: usize) {
        let _resize = self.resize_lock.lock();
        let old = self.limit.swap(new_limit, Ordering::SeqCst);
        if new_limit > old {
            // Growing also cancels any outstanding shrink.
            let grant = new_limit - old;
            // One read-modify-write keeps the debt transition explicit. The
            // resize lock also prevents a return from observing a partial
            // grow while the matching permits are added.
            let cancelled = self
                .debt
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |debt| {
                    Some(debt.saturating_sub(grant))
                })
                // The closure never returns `None`, so this never fails.
                .map_or(0, |before| before.min(grant));
            let grant = grant - cancelled;
            if grant > 0 {
                self.sem.add_permits(grant);
            }
        } else if new_limit < old {
            let owed = old - new_limit;
            // Permit returns cannot interleave these two operations because
            // they take the same guard. Whatever cannot be forgotten here is
            // therefore genuinely in flight and becomes exact shrink debt.
            let forgotten = self.sem.forget_permits(owed);
            if forgotten < owed {
                // The rest are out with in-flight requests; collect them as
                // they come back rather than over-admitting in the meantime.
                self.debt.fetch_add(owed - forgotten, Ordering::SeqCst);
            }
        }
    }

    fn claim_debt(&self) -> bool {
        self.debt
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |debt| {
                debt.checked_sub(1)
            })
            .is_ok()
    }

    pub fn set_queue(&self, max: usize, timeout: Duration) {
        self.queue_max.store(max, Ordering::Relaxed);
        self.queue_timeout_ms
            .store(millis(timeout), Ordering::Relaxed);
    }

    pub fn queue_timeout(&self) -> Duration {
        Duration::from_millis(self.queue_timeout_ms.load(Ordering::Relaxed))
    }

    /// Take a permit, or say why not.
    ///
    /// `deadline` bounds the *total* wait, so a request crossing several
    /// limiters cannot spend each one's queue timeout in turn.
    pub async fn acquire(
        self: &Arc<Self>,
        deadline: Option<Instant>,
    ) -> Result<Permit, ShedReason> {
        if let Ok(p) = self.sem.clone().try_acquire_owned() {
            self.admitted.fetch_add(1, Ordering::Relaxed);
            return Ok(Permit {
                inner: Some(p),
                limiter: self.clone(),
                companion: None,
            });
        }

        let queue_max = self.queue_max.load(Ordering::Relaxed);
        if queue_max == 0 {
            self.shed.fetch_add(1, Ordering::Relaxed);
            return Err(ShedReason::QueueFull);
        }

        // Reserve a queue slot before waiting. An unbounded queue turns an
        // origin overload into a proxy overload and defers the failure rather
        // than preventing it.
        let depth = self.queue_depth.fetch_add(1, Ordering::SeqCst);
        if depth >= queue_max {
            self.queue_depth.fetch_sub(1, Ordering::SeqCst);
            self.shed.fetch_add(1, Ordering::Relaxed);
            return Err(ShedReason::QueueFull);
        }
        self.queued.fetch_add(1, Ordering::Relaxed);
        let queue_slot = QueueSlot {
            limiter: self.clone(),
        };

        let wait = match deadline {
            Some(d) => d.saturating_duration_since(Instant::now()),
            None => self.queue_timeout(),
        };

        let result = tokio::time::timeout(wait, self.sem.clone().acquire_owned()).await;
        drop(queue_slot);

        match result {
            Ok(Ok(p)) => {
                self.admitted.fetch_add(1, Ordering::Relaxed);
                Ok(Permit {
                    inner: Some(p),
                    limiter: self.clone(),
                    companion: None,
                })
            }
            // The semaphore is closed only at shutdown.
            Ok(Err(_)) => {
                self.shed.fetch_add(1, Ordering::Relaxed);
                Err(ShedReason::QueueFull)
            }
            Err(_) => {
                self.shed.fetch_add(1, Ordering::Relaxed);
                Err(ShedReason::QueueTimeout)
            }
        }
    }
}

struct QueueSlot {
    limiter: Arc<Limiter>,
}

impl Drop for QueueSlot {
    fn drop(&mut self) {
        self.limiter.queue_depth.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lim(limit: usize, q: usize, t_ms: u64) -> Arc<Limiter> {
        Limiter::new("test", limit, q, Duration::from_millis(t_ms))
    }

    #[tokio::test]
    async fn admits_up_to_the_limit_and_no_further() {
        let l = lim(2, 0, 0);
        let _a = l.acquire(None).await.unwrap();
        let _b = l.acquire(None).await.unwrap();
        assert_eq!(l.acquire(None).await.unwrap_err(), ShedReason::QueueFull);
    }

    #[tokio::test]
    async fn a_released_permit_admits_the_next_request() {
        let l = lim(1, 0, 0);
        let a = l.acquire(None).await.unwrap();
        assert!(l.acquire(None).await.is_err());
        drop(a);
        assert!(l.acquire(None).await.is_ok());
    }

    #[tokio::test]
    async fn queue_is_bounded_and_refuses_immediately_when_full() {
        let l = lim(1, 1, 5_000);
        let _held = l.acquire(None).await.unwrap();
        let l2 = l.clone();
        // One waiter occupies the single queue slot.
        let waiter = tokio::spawn(async move { l2.acquire(None).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(l.queue_depth(), 1);
        // The next arrival is refused rather than queued.
        assert_eq!(l.acquire(None).await.unwrap_err(), ShedReason::QueueFull);
        waiter.abort();
        let _ = waiter.await;
        assert_eq!(
            l.queue_depth(),
            0,
            "cancelled waiters must return their queue slot"
        );
    }

    #[tokio::test]
    async fn queue_deadline_sheds_rather_than_waiting_forever() {
        let l = lim(1, 10, 50);
        let _held = l.acquire(None).await.unwrap();
        assert_eq!(l.acquire(None).await.unwrap_err(), ShedReason::QueueTimeout);
    }

    #[tokio::test]
    async fn growing_the_limit_admits_more_immediately() {
        let l = lim(1, 0, 0);
        let _a = l.acquire(None).await.unwrap();
        assert!(l.acquire(None).await.is_err());
        l.resize(3);
        assert!(l.acquire(None).await.is_ok());
        assert!(l.acquire(None).await.is_ok());
        assert_eq!(l.limit(), 3);
    }

    #[tokio::test]
    async fn shrinking_never_over_admits_while_requests_are_in_flight() {
        // The reload hazard: 4 permits out, ceiling drops to 1. Returning
        // permits must be absorbed, not handed to new arrivals.
        let l = lim(4, 0, 0);
        let a = l.acquire(None).await.unwrap();
        let b = l.acquire(None).await.unwrap();
        let c = l.acquire(None).await.unwrap();
        let d = l.acquire(None).await.unwrap();

        l.resize(1);
        assert_eq!(l.limit(), 1);

        drop(a);
        assert!(
            l.acquire(None).await.is_err(),
            "still 3 in flight against a ceiling of 1"
        );
        drop(b);
        assert!(
            l.acquire(None).await.is_err(),
            "still 2 in flight against a ceiling of 1"
        );
        drop(c);
        assert!(
            l.acquire(None).await.is_err(),
            "still 1 in flight against a ceiling of 1"
        );
        drop(d);
        assert!(
            l.acquire(None).await.is_ok(),
            "now empty, one permit is correct"
        );
    }

    #[tokio::test]
    async fn growing_cancels_a_pending_shrink() {
        let l = lim(4, 0, 0);
        let a = l.acquire(None).await.unwrap();
        let _b = l.acquire(None).await.unwrap();
        l.resize(1); // owes 2
        l.resize(4); // operator changed their mind
        drop(a);
        assert!(l.acquire(None).await.is_ok());
        assert_eq!(l.limit(), 4);
    }

    /// A grow smaller than the outstanding debt cancels part of it and leaves
    /// the rest owed. The whole debt must not fall on the floor: every permit
    /// still owed has to be absorbed on its way back, or the limiter settles
    /// above the ceiling it was just given.
    #[tokio::test]
    async fn growing_by_less_than_the_debt_leaves_the_remainder_owed() {
        let l = lim(5, 0, 0);
        let a = l.acquire(None).await.unwrap();
        let b = l.acquire(None).await.unwrap();
        let c = l.acquire(None).await.unwrap();
        let d = l.acquire(None).await.unwrap();
        let _e = l.acquire(None).await.unwrap();

        l.resize(1); // owes 4; nothing is available to forget outright
        l.resize(2); // cancels one, three still owed
        assert_eq!(l.limit(), 2);

        for (n, permit) in [a, b, c].into_iter().enumerate() {
            drop(permit);
            assert!(
                l.acquire(None).await.is_err(),
                "return {} of 3 must be absorbed against the remaining debt",
                n + 1
            );
        }

        drop(d);
        assert!(
            l.acquire(None).await.is_ok(),
            "the debt is settled, so this return is the ceiling's own permit"
        );
    }

    #[tokio::test]
    async fn shrinking_absorbs_a_returned_permit_before_a_waiter_can_take_it() {
        let l = lim(2, 1, 5_000);
        let a = l.acquire(None).await.unwrap();
        let b = l.acquire(None).await.unwrap();
        let waiting_limiter = l.clone();
        let mut waiter = tokio::spawn(async move { waiting_limiter.acquire(None).await });
        tokio::time::sleep(Duration::from_millis(20)).await;

        l.resize(1);
        drop(a);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut waiter)
                .await
                .is_err(),
            "the shrink debt was handed to a queued request"
        );

        drop(b);
        assert!(waiter.await.unwrap().is_ok());
    }
}
