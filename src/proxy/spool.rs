//! A bounded buffer that decouples the origin from a slow client.
//!
//! # The problem it solves
//!
//! An origin work permit is supposed to represent *render* capacity: hold one
//! while the origin is producing a response, give it back when the origin has
//! finished. Pingora makes the second half hard. Its proxy loop pairs an
//! upstream read with a downstream write through a four-slot channel, so the
//! origin can never run more than four chunks ahead of the client. A client
//! that reads a 4 MiB page one kilobyte at a time therefore keeps the upstream
//! side of that pairing alive, and `end_of_stream` — the only honest signal
//! that the origin stopped rendering — does not arrive until the client is
//! done. The permit is held for the client's reading time, not the origin's
//! rendering time.
//!
//! Measured on this codebase: a 1 MiB body returned capacity in 91 ms; a 2 MiB
//! body against a rate-limited reader held the permit until the request was
//! shed three seconds later. The threshold is wherever the socket buffers
//! between the two ends stop absorbing the difference.
//!
//! # How this fixes it
//!
//! [`ProxyHttp::response_body_filter`](pingora_proxy::ProxyHttp::response_body_filter)
//! runs on the downstream side of that channel, immediately *before* the write
//! that would block. Taking the bytes there and returning nothing makes the
//! write trivial, so the loop comes straight back for the next upstream chunk
//! and the origin runs at full speed. When the upstream reports end of stream,
//! the origin has genuinely finished — no downstream backpressure was ever
//! applied to it — and the permit can go back. The buffered body is then
//! handed downstream in one task and drains at whatever pace the client
//! manages, with no origin capacity attached to it.
//!
//! The hook choice matters: the same trick in `upstream_response_body_filter`
//! would run *before* Pingora writes the body to the cache, and would store an
//! empty entry.
//!
//! # What it costs
//!
//! Progressive rendering. A spooled response reaches the client only once the
//! origin has finished producing it, so a streamed SSR shell no longer arrives
//! early. That is a real regression for streaming routes and is why spooling
//! is off by default, configured per route, and never applied to a request
//! that holds no permit — a `class: streaming` route is exempt from admission
//! and has nothing to gain here.
//!
//! # What bounds it
//!
//! Two ceilings, because one is not enough. `spool.max_body` bounds a single
//! response; `spool.max_memory` bounds every in-flight spool at once, which is
//! what stops a thousand slow readers from turning a 2 MiB per-request bound
//! into 2 GiB of resident memory. Exceeding either is not an error: the
//! buffered bytes are flushed, the rest of the body streams through as it did
//! before, and the permit reverts to being bounded by
//! `timeouts.downstream_write`. Degrading to the old behaviour is always safe;
//! refusing the response would not be.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::{Bytes, BytesMut};

/// The process-wide memory ceiling for spooling, shared by every request.
#[derive(Debug)]
pub struct SpoolBudget {
    limit: usize,
    used: AtomicUsize,
}

impl SpoolBudget {
    pub fn new(limit: usize) -> Arc<SpoolBudget> {
        Arc::new(SpoolBudget {
            limit,
            used: AtomicUsize::new(0),
        })
    }

    pub fn used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Claim `bytes` of the global budget, or refuse.
    ///
    /// Compare-and-swap rather than `fetch_add` and check: a `fetch_add` that
    /// overshoots has already published the overshoot, and two requests racing
    /// at the ceiling would each see the other's addition and both back out,
    /// leaving the budget wrong until the next release.
    fn reserve(&self, bytes: usize) -> bool {
        let mut used = self.used.load(Ordering::Relaxed);
        loop {
            let next = match used.checked_add(bytes) {
                Some(next) if next <= self.limit => next,
                _ => return false,
            };
            match self
                .used
                .compare_exchange_weak(used, next, Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => return true,
                Err(observed) => used = observed,
            }
        }
    }

    fn release(&self, bytes: usize) {
        self.used.fetch_sub(bytes, Ordering::AcqRel);
    }
}

/// Why a request stopped spooling. Recorded for the access log and metrics so
/// an operator can tell "the spool worked" from "the spool gave up".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpoolOutcome {
    /// Absorbed the whole body; the permit was released at the origin's own
    /// end of stream.
    Complete,
    /// The response outgrew `spool.max_body`.
    BodyTooLarge,
    /// `spool.max_memory` was already committed to other requests.
    BudgetExhausted,
}

impl SpoolOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            SpoolOutcome::Complete => "complete",
            SpoolOutcome::BodyTooLarge => "body_too_large",
            SpoolOutcome::BudgetExhausted => "budget_exhausted",
        }
    }
}

/// One request's buffer.
///
/// Reservations are taken from the global budget in chunks as the body grows
/// rather than all at once up front: reserving `max_body` for every spooled
/// request would let a handful of small pages exhaust a budget none of them
/// needed.
#[derive(Debug)]
pub struct Spool {
    budget: Arc<SpoolBudget>,
    buffer: BytesMut,
    /// How much of the global budget this request currently holds. Always
    /// `>= buffer.len()`, and always returned on drop.
    reserved: usize,
    max_body: usize,
    /// Once this is set the spool is inert: everything it holds has been
    /// handed downstream and later chunks pass straight through.
    gave_up: Option<SpoolOutcome>,
}

/// How much budget to claim at a time. Small enough that a short response does
/// not lock up a megabyte, large enough that a chunked body does not perform a
/// contended atomic per 8 KiB frame.
const RESERVE_STEP: usize = 64 * 1024;

impl Spool {
    pub fn new(budget: Arc<SpoolBudget>, max_body: usize) -> Spool {
        Spool {
            budget,
            buffer: BytesMut::new(),
            reserved: 0,
            max_body,
            gave_up: None,
        }
    }

    pub fn buffered(&self) -> usize {
        self.buffer.len()
    }

    pub fn outcome(&self) -> Option<SpoolOutcome> {
        self.gave_up
    }

    /// Is this spool still absorbing?
    pub fn is_active(&self) -> bool {
        self.gave_up.is_none()
    }

    /// Offer one body chunk.
    ///
    /// Returns what should be written downstream *now*: `None` while the
    /// chunk is being absorbed, `Some(bytes)` when the spool is flushing —
    /// either because the body ended or because it gave up.
    ///
    /// The `end_of_stream` flush is the point of the whole exercise: by then
    /// the permit has already been released, because the origin was never
    /// made to wait on the client.
    pub fn offer(&mut self, chunk: Option<Bytes>, end_of_stream: bool) -> Option<Bytes> {
        if self.gave_up.is_some() {
            // Inert: whatever we held has already gone downstream.
            return chunk;
        }

        let incoming = chunk.as_ref().map_or(0, Bytes::len);
        if incoming > 0 {
            if self.buffer.len() + incoming > self.max_body {
                return self.give_up(SpoolOutcome::BodyTooLarge, chunk);
            }
            if !self.ensure_reserved(self.buffer.len() + incoming) {
                return self.give_up(SpoolOutcome::BudgetExhausted, chunk);
            }
            // `incoming > 0` already proved the chunk is present; matching
            // again is how that is stated without a panic branch.
            if let Some(bytes) = chunk.as_ref() {
                self.buffer.extend_from_slice(bytes);
            }
        }

        if end_of_stream {
            self.gave_up = Some(SpoolOutcome::Complete);
            return self.drain();
        }
        None
    }

    /// Stop absorbing. Hand back everything buffered *plus* the chunk that did
    /// not fit, in that order, so the body reaches the client unreordered.
    fn give_up(&mut self, why: SpoolOutcome, chunk: Option<Bytes>) -> Option<Bytes> {
        self.gave_up = Some(why);
        let mut out = self.drain().unwrap_or_default();
        if let Some(chunk) = chunk {
            let mut joined = BytesMut::with_capacity(out.len() + chunk.len());
            joined.extend_from_slice(&out);
            joined.extend_from_slice(&chunk);
            out = joined.freeze();
        }
        (!out.is_empty()).then_some(out)
    }

    /// Hand the buffer downstream, keeping the reservation.
    ///
    /// Deliberately *not* releasing here. Once the spool flushes, the bytes
    /// move into Pingora's downstream write buffer and stay resident until the
    /// slow client has drained them — which is the whole reason the spool
    /// exists, so it is also exactly when the memory matters. Releasing at
    /// flush would make `spool.max_memory` a bound on nothing: a thousand slow
    /// readers would each flush, each free their accounting, and each keep
    /// their megabytes. The reservation is returned on drop, when the request
    /// is genuinely over.
    fn drain(&mut self) -> Option<Bytes> {
        let out = std::mem::take(&mut self.buffer).freeze();
        (!out.is_empty()).then_some(out)
    }

    fn ensure_reserved(&mut self, needed: usize) -> bool {
        if needed <= self.reserved {
            return true;
        }
        let unreserved = needed - self.reserved;
        let rounded = unreserved.saturating_add(RESERVE_STEP - 1) / RESERVE_STEP * RESERVE_STEP;
        // A configured body ceiling smaller than RESERVE_STEP is valid. Do
        // not ask the global budget for 64 KiB when this response can retain
        // at most (for example) 1 KiB.
        let step = rounded.min(self.max_body.saturating_sub(self.reserved));
        if !self.budget.reserve(step) {
            return false;
        }
        self.reserved += step;
        true
    }

    fn release_all(&mut self) {
        if self.reserved > 0 {
            self.budget.release(self.reserved);
            self.reserved = 0;
        }
    }
}

/// A client that disconnects mid-spool, or a request abandoned anywhere else
/// in the pipeline, must not leak its share of the global budget.
impl Drop for Spool {
    fn drop(&mut self) {
        self.release_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(len: usize) -> Option<Bytes> {
        Some(Bytes::from(vec![b'x'; len]))
    }

    #[test]
    fn a_body_that_fits_is_withheld_until_end_of_stream() {
        let budget = SpoolBudget::new(1024 * 1024);
        let mut spool = Spool::new(budget, 64 * 1024);

        assert_eq!(spool.offer(chunk(1000), false), None);
        assert_eq!(spool.offer(chunk(1000), false), None);
        assert_eq!(spool.buffered(), 2000);

        let flushed = spool
            .offer(chunk(500), true)
            .expect("end of stream flushes");
        assert_eq!(flushed.len(), 2500);
        assert_eq!(spool.outcome(), Some(SpoolOutcome::Complete));
    }

    #[test]
    fn the_budget_is_held_past_the_flush_and_returned_when_the_request_ends() {
        // Flushing moves the bytes into Pingora's write buffer; it does not
        // free them. A budget released at flush would bound nothing, because
        // the case it exists for — a slow client — is precisely the case where
        // the bytes stay resident long after the flush.
        let budget = SpoolBudget::new(1024 * 1024);
        {
            let mut spool = Spool::new(budget.clone(), 64 * 1024);
            spool.offer(chunk(1000), false);
            assert!(budget.used() > 0, "nothing was reserved");
            spool.offer(None, true);
            assert!(
                budget.used() > 0,
                "budget was released at flush, while the bytes were still resident downstream"
            );
        }
        assert_eq!(
            budget.used(),
            0,
            "budget still held after the request ended"
        );
    }

    #[test]
    fn an_oversized_body_flushes_in_order_and_then_passes_through() {
        // Order is the property that matters: giving up must not reorder the
        // body, or the client receives a corrupted document rather than a
        // slow one.
        let budget = SpoolBudget::new(1024 * 1024);
        let mut spool = Spool::new(budget, 1000);

        assert_eq!(spool.offer(Some(Bytes::from_static(b"aaa")), false), None);
        let flushed = spool
            .offer(Some(Bytes::from(vec![b'b'; 1500])), false)
            .expect("giving up flushes what was held");
        assert_eq!(&flushed[..3], b"aaa");
        assert_eq!(flushed.len(), 1503);
        assert_eq!(spool.outcome(), Some(SpoolOutcome::BodyTooLarge));

        // And from here it is a pass-through, not a black hole.
        let passed = spool.offer(Some(Bytes::from_static(b"ccc")), true);
        assert_eq!(passed.as_deref(), Some(&b"ccc"[..]));
    }

    #[test]
    fn an_exhausted_global_budget_degrades_instead_of_failing() {
        // One request holds the whole budget; the next must still be served,
        // just without the spool's benefit.
        let budget = SpoolBudget::new(RESERVE_STEP);
        let mut first = Spool::new(budget.clone(), 1024 * 1024);
        assert_eq!(first.offer(chunk(1000), false), None);

        let mut second = Spool::new(budget.clone(), 1024 * 1024);
        let passed = second
            .offer(chunk(1000), false)
            .expect("a refused reservation must pass the chunk through");
        assert_eq!(passed.len(), 1000);
        assert_eq!(second.outcome(), Some(SpoolOutcome::BudgetExhausted));
    }

    #[test]
    fn dropping_a_spool_returns_its_reservation() {
        // The disconnect path: a client that vanishes mid-body never reaches
        // end of stream, so the only thing that can free the budget is Drop.
        let budget = SpoolBudget::new(1024 * 1024);
        {
            let mut spool = Spool::new(budget.clone(), 64 * 1024);
            spool.offer(chunk(4000), false);
            assert!(budget.used() > 0);
        }
        assert_eq!(budget.used(), 0, "an abandoned request leaked its budget");
    }

    #[test]
    fn an_empty_body_produces_no_downstream_write() {
        let budget = SpoolBudget::new(1024 * 1024);
        let mut spool = Spool::new(budget, 64 * 1024);
        assert_eq!(spool.offer(None, true), None);
        assert_eq!(spool.outcome(), Some(SpoolOutcome::Complete));
    }

    #[test]
    fn a_body_exactly_at_the_ceiling_is_still_spooled() {
        let budget = SpoolBudget::new(1024 * 1024);
        let mut spool = Spool::new(budget, 1000);
        assert_eq!(spool.offer(chunk(1000), false), None);
        assert_eq!(spool.offer(None, true).map(|b| b.len()), Some(1000));
        assert_eq!(spool.outcome(), Some(SpoolOutcome::Complete));
    }

    #[test]
    fn a_budget_smaller_than_the_reservation_step_still_spools() {
        let budget = SpoolBudget::new(1024);
        let mut spool = Spool::new(budget.clone(), 1024);

        assert_eq!(spool.offer(chunk(512), false), None);
        assert_eq!(budget.used(), 1024);
        assert_eq!(
            spool.offer(chunk(512), true).map(|bytes| bytes.len()),
            Some(1024)
        );
        assert_eq!(spool.outcome(), Some(SpoolOutcome::Complete));
    }

    #[test]
    fn the_global_budget_is_never_overcommitted_under_contention() {
        use std::thread;
        const LIMIT: usize = 16 * RESERVE_STEP;
        let budget = SpoolBudget::new(LIMIT);

        let threads: Vec<_> = (0..8)
            .map(|_| {
                let budget = budget.clone();
                thread::spawn(move || {
                    let mut held = Vec::new();
                    for _ in 0..20 {
                        let mut spool = Spool::new(budget.clone(), 1024 * 1024);
                        spool.offer(chunk(RESERVE_STEP), false);
                        assert!(
                            budget.used() <= LIMIT,
                            "budget overcommitted: {} > {LIMIT}",
                            budget.used()
                        );
                        held.push(spool);
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(budget.used(), 0, "budget leaked after every spool dropped");
    }
}
