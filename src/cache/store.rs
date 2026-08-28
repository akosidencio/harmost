//! A bounded in-memory implementation of Pingora's `Storage` trait.
//!
//! Pingora ships exactly one `Storage` implementation, `MemCache`, and it is
//! documented "for testing only, not for production use" for two concrete
//! reasons: it is an unbounded `HashMap`, and its `lookup_streaming_write` is
//! `.expect("must have partial write in progress")` — so a race between the
//! cache lock releasing and a late waiter's lookup takes the process down.
//! This implementation has a byte budget and returns a miss instead of
//! panicking.
//!
//! Waiters attach to a write *in progress* via the streaming-write tag, so a
//! coalesced request receives the leader's first chunk as the leader produces
//! it rather than waiting for the whole render. That is what keeps request
//! collapsing from destroying streaming.
//!
//! Both Pingora's per-response size limit and this store's global budget are
//! enforced while a fill is in progress. Abandoned fills remove their temporary
//! entry on drop.

use async_trait::async_trait;
use bytes::Bytes;
use parking_lot::{Mutex, RwLock};
use pingora_cache::CacheMeta;
use pingora_cache::key::{CacheHashKey, CacheKey, CompactCacheKey};
use pingora_cache::storage::{
    HandleHit, HandleMiss, HitHandler, MissFinishType, MissHandler, PurgeType, Storage,
    streaming_write::U64WriteId,
};
use pingora_cache::trace::SpanHandle;
use pingora_error::{Error, ErrorType, Result};
use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::watch;

type BinaryMeta = (Vec<u8>, Vec<u8>);

/// Pingora wakes cache-lock readers and makes them perform another ordinary
/// lookup. A small handoff window keeps a just-completed transient response
/// visible long enough for those already-woken tasks to attach, without
/// turning it into a persistent cache entry.
const TRANSIENT_HANDOFF: std::time::Duration = std::time::Duration::from_millis(25);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Written {
    Partial(usize),
    Complete(usize),
}

impl Written {
    fn bytes(self) -> usize {
        match self {
            Written::Partial(n) | Written::Complete(n) => n,
        }
    }
    fn done(self) -> bool {
        matches!(self, Written::Complete(_))
    }
}

/// A finished entry.
#[derive(Clone)]
struct Stored {
    meta: BinaryMeta,
    body: Arc<RwLock<Vec<u8>>>,
}

impl Stored {
    fn size(&self) -> usize {
        self.meta.0.len() + self.meta.1.len() + self.body.read().len()
    }
}

/// A write in progress. Readers attach to this and follow it.
struct Temp {
    meta: BinaryMeta,
    body: Arc<RwLock<Vec<u8>>>,
    written: Arc<watch::Sender<Written>>,
    transient: bool,
}

impl Temp {
    fn new(meta: BinaryMeta, transient: bool) -> Self {
        let (tx, _rx) = watch::channel(Written::Partial(0));
        Temp {
            meta,
            body: Arc::new(RwLock::new(Vec::new())),
            written: Arc::new(tx),
            transient,
        }
    }
}

/// An in-memory store with a byte budget.
///
/// The budget is the whole point: `MemCache` is an unbounded map, which is the
/// substantive reason it is not for production, and a cache that cannot be
/// bounded is a memory-exhaustion vector in a component whose job is to absorb
/// traffic spikes.
pub struct BoundedStore {
    cached: RwLock<HashMap<String, Stored>>,
    /// Insertion order, for eviction. A real implementation would use
    /// TinyUFO; FIFO is enough to prove the budget is enforceable.
    order: RwLock<VecDeque<String>>,
    temp: RwLock<HashMap<String, HashMap<u64, Temp>>>,
    /// Includes completed entries and every byte in an in-progress fill.
    /// All memory-accounting mutations take this lock before cache-map locks.
    used: Mutex<usize>,
    max_bytes: usize,
    next_id: AtomicU64,
}

impl BoundedStore {
    pub fn new(max_bytes: usize) -> &'static Self {
        // `Storage` takes `&'static self` throughout (upstream marks this a
        // TODO). Leaking once at startup is the intended shape.
        Box::leak(Box::new(BoundedStore {
            cached: RwLock::new(HashMap::new()),
            order: RwLock::new(VecDeque::new()),
            temp: RwLock::new(HashMap::new()),
            used: Mutex::new(0),
            max_bytes,
            next_id: AtomicU64::new(0),
        }))
    }

    pub fn bytes_used(&self) -> usize {
        *self.used.lock()
    }

    /// The configured byte budget. Published in the admin status document
    /// next to `bytes_used`, because occupancy without its ceiling is a number
    /// nobody can act on.
    pub fn limit(&self) -> usize {
        self.max_bytes
    }

    pub fn entries(&self) -> usize {
        self.cached.read().len()
    }

    fn reserve(&self, additional: usize) -> bool {
        if additional > self.max_bytes {
            return false;
        }
        let mut used = self.used.lock();
        let mut cached = self.cached.write();
        let mut order = self.order.write();
        while used.saturating_add(additional) > self.max_bytes {
            let Some(victim) = order.pop_front() else {
                return false;
            };
            if let Some(entry) = cached.remove(&victim) {
                *used = used.saturating_sub(entry.size());
            }
        }
        *used += additional;
        true
    }

    fn release(&self, amount: usize) {
        let mut used = self.used.lock();
        *used = used.saturating_sub(amount);
    }

    /// Convert an already-accounted temporary fill into a completed entry.
    fn admit(&self, key: String, obj: Stored) {
        let mut used = self.used.lock();
        let mut cached = self.cached.write();
        let mut order = self.order.write();
        if let Some(old) = cached.insert(key.clone(), obj) {
            *used = used.saturating_sub(old.size());
            order.retain(|existing| existing != &key);
        }
        order.push_back(key);
    }

    fn remove_temp(&self, key: &str, id: u64) -> Option<Temp> {
        let mut temp = self.temp.write();
        let removed = temp.get_mut(key).and_then(|writes| writes.remove(&id));
        if temp.get(key).is_some_and(HashMap::is_empty) {
            temp.remove(key);
        }
        removed
    }
}

#[async_trait]
impl Storage for BoundedStore {
    async fn lookup(
        &'static self,
        key: &CacheKey,
        _t: &SpanHandle,
    ) -> Result<Option<(CacheMeta, HitHandler)>> {
        let hash = key.combined();

        if let Some(obj) = self.cached.read().get(&hash).cloned() {
            let meta = CacheMeta::deserialize(&obj.meta.0, &obj.meta.1)?;
            return Ok(Some((
                meta,
                Box::new(CompleteHit {
                    body: obj.body,
                    read: 0,
                }),
            )));
        }

        // Nothing finished — but a write may be in progress, and this is the
        // path a coalesced waiter arrives on.
        //
        // When a storage supports streaming partial writes, Pingora releases
        // the cache lock as soon as the leader's miss handler exists, so
        // waiters wake with `LockStatus::Done` and come straight back here
        // with no write tag. If `lookup` only ever answered from finished
        // entries, every one of them would miss and go to the origin — which
        // is request collapsing failing silently, in the one shape where the
        // collapsing matters most.
        let partial = {
            let temp = self.temp.read();
            temp.get(&hash).and_then(|writes| {
                // One leader per key in practice; take the newest if a
                // revalidation overlaps.
                writes
                    .iter()
                    .max_by_key(|(id, _)| **id)
                    .map(|(_, t)| (t.meta.clone(), t.body.clone(), t.written.subscribe()))
            })
        };
        let Some((meta, body, written)) = partial else {
            return Ok(None);
        };
        let meta = CacheMeta::deserialize(&meta.0, &meta.1)?;
        Ok(Some((
            meta,
            Box::new(PartialHit {
                body,
                written,
                read: 0,
            }),
        )))
    }

    async fn lookup_streaming_write(
        &'static self,
        key: &CacheKey,
        streaming_write_tag: Option<&[u8]>,
        t: &SpanHandle,
    ) -> Result<Option<(CacheMeta, HitHandler)>> {
        let Some(tag) = streaming_write_tag else {
            return self.lookup(key, t).await;
        };
        let Ok(write_id): std::result::Result<U64WriteId, _> = tag.try_into() else {
            // MemCache panics here. A malformed tag is not a reason to take
            // the process down.
            return Ok(None);
        };
        let id: u64 = write_id.into();
        let hash = key.combined();

        // Take everything needed out of the lock in one scope: a parking_lot
        // guard is !Send, so holding one across the await below makes the
        // whole future non-Send.
        let found = {
            let guard = self.temp.read();
            guard.get(&hash).and_then(|m| m.get(&id)).map(|temp| {
                (
                    temp.meta.clone(),
                    temp.body.clone(),
                    temp.written.subscribe(),
                )
            })
        };
        let Some((meta, body, written)) = found else {
            // Pingora requires an exact tag match here. Falling back to another
            // completed or in-progress write could attach the leader itself to
            // a different response for the same key.
            return Ok(None);
        };
        let meta = CacheMeta::deserialize(&meta.0, &meta.1)?;
        Ok(Some((
            meta,
            Box::new(PartialHit {
                body,
                written,
                read: 0,
            }),
        )))
    }

    async fn get_miss_handler(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        _t: &SpanHandle,
    ) -> Result<MissHandler> {
        let hash = key.combined();
        let transient = meta
            .response_header()
            .headers
            .contains_key(crate::cache::TRANSIENT_HEADER);
        let serialized = meta.serialize()?;
        let meta_size = serialized.0.len() + serialized.1.len();
        if !self.reserve(meta_size) {
            return Error::e_explain(ErrorType::InternalError, "cache memory budget exhausted");
        }
        let temp = Temp::new(serialized, transient);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let handler = FollowableMiss {
            store: self,
            key: hash.clone(),
            id: id.into(),
            body: temp.body.clone(),
            written: temp.written.clone(),
            accounted: meta_size,
            finished: false,
        };
        self.temp.write().entry(hash).or_default().insert(id, temp);
        Ok(Box::new(handler))
    }

    async fn purge(
        &'static self,
        key: &CompactCacheKey,
        _p: PurgeType,
        _t: &SpanHandle,
    ) -> Result<bool> {
        let hash = key.combined();
        let removed = self.cached.write().remove(&hash);
        if let Some(v) = &removed {
            self.release(v.size());
            self.order.write().retain(|k| k != &hash);
        }
        Ok(removed.is_some())
    }

    async fn update_meta(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        _t: &SpanHandle,
    ) -> Result<bool> {
        let hash = key.combined();
        let replacement = meta.serialize()?;
        let replacement_size = replacement.0.len() + replacement.1.len();
        let mut used = self.used.lock();
        let mut cached = self.cached.write();
        let Some(obj) = cached.get_mut(&hash) else {
            return Ok(false);
        };
        let old_size = obj.meta.0.len() + obj.meta.1.len();
        if replacement_size > old_size {
            let growth = replacement_size - old_size;
            if used.saturating_add(growth) > self.max_bytes {
                return Ok(false);
            }
            *used += growth;
        } else {
            *used = used.saturating_sub(old_size - replacement_size);
        }
        obj.meta = replacement;
        Ok(true)
    }

    fn support_streaming_partial_write(&self) -> bool {
        true
    }

    fn as_any(&self) -> &(dyn Any + Send + Sync + 'static) {
        self
    }
}

/// Reads a finished entry.
struct CompleteHit {
    body: Arc<RwLock<Vec<u8>>>,
    read: usize,
}

#[async_trait]
impl HandleHit for CompleteHit {
    async fn read_body(&mut self) -> Result<Option<Bytes>> {
        const CHUNK: usize = 64 * 1024;
        let body = self.body.read();
        if self.read >= body.len() {
            return Ok(None);
        }
        let end = (self.read + CHUNK).min(body.len());
        let chunk = Bytes::copy_from_slice(&body[self.read..end]);
        self.read = end;
        Ok(Some(chunk))
    }

    async fn finish(
        self: Box<Self>,
        _storage: &'static (dyn Storage + Sync),
        _key: &CacheKey,
        _t: &SpanHandle,
    ) -> Result<()> {
        Ok(())
    }

    fn can_seek(&self) -> bool {
        true
    }

    fn seek(&mut self, start: usize, _end: Option<usize>) -> Result<()> {
        self.read = start.min(self.body.read().len());
        Ok(())
    }

    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self
    }

    fn as_any_mut(&mut self) -> &mut (dyn Any + Send + Sync) {
        self
    }
}

/// Reads a write that is still in progress, following it chunk by chunk.
///
/// This is what decides whether coalescing destroys streaming. A waiter here
/// receives the leader's first chunk as soon as the leader produces it, rather
/// than waiting for the whole render to finish.
struct PartialHit {
    body: Arc<RwLock<Vec<u8>>>,
    written: watch::Receiver<Written>,
    read: usize,
}

#[async_trait]
impl HandleHit for PartialHit {
    async fn read_body(&mut self) -> Result<Option<Bytes>> {
        loop {
            let state = *self.written.borrow_and_update();
            let available = state.bytes();
            if available > self.read {
                // Copy out under the lock, then release it before awaiting.
                let end = (self.read + 64 * 1024).min(available);
                let chunk = Bytes::copy_from_slice(&self.body.read()[self.read..end]);
                self.read = end;
                return Ok(Some(chunk));
            }
            if state.done() {
                return Ok(None);
            }
            if self.written.changed().await.is_err() {
                // The writer vanished without finishing.
                return Err(Error::explain(
                    ErrorType::InternalError,
                    "cache writer dropped",
                ));
            }
        }
    }

    async fn finish(
        self: Box<Self>,
        _storage: &'static (dyn Storage + Sync),
        _key: &CacheKey,
        _t: &SpanHandle,
    ) -> Result<()> {
        Ok(())
    }

    fn can_seek(&self) -> bool {
        false
    }

    fn as_any(&self) -> &(dyn Any + Send + Sync) {
        self
    }

    fn as_any_mut(&mut self) -> &mut (dyn Any + Send + Sync) {
        self
    }
}

/// The leader's writer. Publishes progress so waiters can follow.
struct FollowableMiss {
    store: &'static BoundedStore,
    key: String,
    id: U64WriteId,
    body: Arc<RwLock<Vec<u8>>>,
    written: Arc<watch::Sender<Written>>,
    accounted: usize,
    finished: bool,
}

#[async_trait]
impl HandleMiss for FollowableMiss {
    async fn write_body(&mut self, data: Bytes, eof: bool) -> Result<()> {
        if !data.is_empty() && !self.store.reserve(data.len()) {
            return Error::e_explain(
                ErrorType::InternalError,
                "cache memory budget exhausted during fill",
            );
        }
        self.accounted += data.len();
        let so_far = self.written.borrow().bytes();
        self.body.write().extend_from_slice(&data);
        let total = so_far + data.len();
        self.written.send_replace(if eof {
            Written::Complete(total)
        } else {
            Written::Partial(total)
        });
        Ok(())
    }

    async fn finish(mut self: Box<Self>) -> Result<MissFinishType> {
        let id: u64 = self.id.into();
        // Make sure any follower that has not drained yet sees the end.
        let total = self.written.borrow().bytes();
        self.written.send_replace(Written::Complete(total));

        let transient = {
            let temp = self.store.temp.read();
            temp.get(&self.key)
                .and_then(|writes| writes.get(&id))
                .map(|entry| entry.transient)
        };
        let Some(transient) = transient else {
            return Err(Error::explain(ErrorType::InternalError, "write vanished"));
        };
        let size = self.body.read().len();
        self.finished = true;
        if transient {
            // The lock is released when the miss handler is created. If the
            // whole body arrives in the same upstream read as the headers,
            // removing this temp immediately races the woken followers and
            // turns each into a new origin request. Keep it only for a short
            // scheduler handoff; attached readers retain their own Arcs.
            let store = self.store;
            let key = self.key.clone();
            let accounted = self.accounted;
            tokio::spawn(async move {
                tokio::time::sleep(TRANSIENT_HANDOFF).await;
                if store.remove_temp(&key, id).is_some() {
                    store.release(accounted);
                }
            });
        } else {
            let temp = self
                .store
                .remove_temp(&self.key, id)
                .ok_or_else(|| Error::explain(ErrorType::InternalError, "write vanished"))?;
            let body = temp.body;
            self.store.admit(
                self.key.clone(),
                Stored {
                    meta: temp.meta,
                    body,
                },
            );
        }
        Ok(MissFinishType::Created(size))
    }

    fn streaming_write_tag(&self) -> Option<&[u8]> {
        Some(self.id.as_bytes())
    }
}

impl Drop for FollowableMiss {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let id: u64 = self.id.into();
        if self.store.remove_temp(&self.key, id).is_some() {
            self.store.release(self.accounted);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pingora_cache::CacheMeta;
    use pingora_cache::trace::Span;
    use pingora_http::ResponseHeader;
    use std::time::{Duration, SystemTime};

    fn meta() -> CacheMeta {
        let now = SystemTime::now();
        CacheMeta::new(
            now + Duration::from_secs(60),
            now,
            0,
            0,
            ResponseHeader::build(200, None).unwrap(),
        )
    }

    #[tokio::test]
    async fn lookup_serves_a_write_that_is_still_in_progress() {
        // The regression this guards: Pingora releases the cache lock as soon
        // as the leader's miss handler exists, so coalesced waiters arrive at
        // `lookup` with no write tag while the body is still being written.
        // A `lookup` that only answers from finished entries makes every
        // waiter miss and go to the origin — request collapsing failing
        // silently on exactly the streaming responses it matters most for.
        let store = BoundedStore::new(1 << 20);
        let key = CacheKey::new("", "/streaming", "");

        let mut leader = store
            .get_miss_handler(&key, &meta(), &Span::inactive().handle())
            .await
            .unwrap();
        leader
            .write_body(Bytes::from_static(b"shell"), false)
            .await
            .unwrap();

        let hit = store
            .lookup(&key, &Span::inactive().handle())
            .await
            .unwrap();
        let (_meta, mut reader) =
            hit.expect("a waiter must be able to attach to the in-flight write");
        assert_eq!(&reader.read_body().await.unwrap().unwrap()[..], b"shell");
    }

    #[tokio::test]
    async fn lookup_prefers_a_finished_entry_over_a_new_write() {
        let store = BoundedStore::new(1 << 20);
        let key = CacheKey::new("", "/revalidating", "");

        let mut first = store
            .get_miss_handler(&key, &meta(), &Span::inactive().handle())
            .await
            .unwrap();
        first
            .write_body(Bytes::from_static(b"old"), true)
            .await
            .unwrap();
        first.finish().await.unwrap();

        // A revalidation starts while the finished entry is still servable.
        let mut second = store
            .get_miss_handler(&key, &meta(), &Span::inactive().handle())
            .await
            .unwrap();
        second
            .write_body(Bytes::from_static(b"new"), false)
            .await
            .unwrap();

        let (_m, mut reader) = store
            .lookup(&key, &Span::inactive().handle())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            &reader.read_body().await.unwrap().unwrap()[..],
            b"old",
            "a complete entry should win over a half-written one"
        );
    }

    #[tokio::test]
    async fn lookup_still_misses_when_there_is_nothing_at_all() {
        let store = BoundedStore::new(1 << 20);
        let key = CacheKey::new("", "/absent", "");
        assert!(
            store
                .lookup(&key, &Span::inactive().handle())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn dropping_an_incomplete_fill_releases_memory_and_removes_the_temp_entry() {
        let store = BoundedStore::new(1 << 20);
        let key = CacheKey::new("", "/abandoned", "");
        let mut writer = store
            .get_miss_handler(&key, &meta(), &Span::inactive().handle())
            .await
            .unwrap();
        writer
            .write_body(Bytes::from_static(b"partial"), false)
            .await
            .unwrap();
        assert!(store.bytes_used() > 0);

        drop(writer);

        assert_eq!(store.bytes_used(), 0);
        assert!(
            store
                .lookup(&key, &Span::inactive().handle())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_streaming_tag_never_falls_back_to_an_unrelated_entry() {
        let store = BoundedStore::new(1 << 20);
        let key = CacheKey::new("", "/tag", "");
        let mut writer = store
            .get_miss_handler(&key, &meta(), &Span::inactive().handle())
            .await
            .unwrap();
        writer
            .write_body(Bytes::from_static(b"done"), true)
            .await
            .unwrap();
        let stale_tag = writer.streaming_write_tag().unwrap().to_vec();
        writer.finish().await.unwrap();

        assert!(
            store
                .lookup_streaming_write(&key, Some(&stale_tag), &Span::inactive().handle())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn transient_fills_exist_only_for_the_follower_handoff() {
        let store = BoundedStore::new(1 << 20);
        let key = CacheKey::new("", "/transient", "");
        let now = SystemTime::now();
        let mut header = ResponseHeader::build(200, None).unwrap();
        header
            .insert_header(crate::cache::TRANSIENT_HEADER, "1")
            .unwrap();
        let transient = CacheMeta::new(now + Duration::from_secs(30), now, 0, 0, header);
        let mut writer = store
            .get_miss_handler(&key, &transient, &Span::inactive().handle())
            .await
            .unwrap();
        writer
            .write_body(Bytes::from_static(b"one flight"), true)
            .await
            .unwrap();
        writer.finish().await.unwrap();

        assert!(
            store
                .lookup(&key, &Span::inactive().handle())
                .await
                .unwrap()
                .is_some()
        );
        tokio::time::sleep(TRANSIENT_HANDOFF + Duration::from_millis(25)).await;
        assert_eq!(store.bytes_used(), 0);
        assert!(
            store
                .lookup(&key, &Span::inactive().handle())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn in_progress_fills_count_toward_the_global_budget() {
        let store = BoundedStore::new(4096);
        let first = CacheKey::new("", "/one", "");
        let second = CacheKey::new("", "/two", "");
        let mut a = store
            .get_miss_handler(&first, &meta(), &Span::inactive().handle())
            .await
            .unwrap();
        a.write_body(Bytes::from(vec![b'a'; 3000]), false)
            .await
            .unwrap();
        let mut b = store
            .get_miss_handler(&second, &meta(), &Span::inactive().handle())
            .await
            .unwrap();
        assert!(
            b.write_body(Bytes::from(vec![b'b'; 3000]), false)
                .await
                .is_err()
        );
        assert!(store.bytes_used() <= 4096);
        drop(a);
        drop(b);
        assert_eq!(store.bytes_used(), 0);
    }
}
