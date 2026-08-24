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
//! Response size limits are not enforced here: `HttpCache::set_max_file_size_bytes`
//! already tracks body bytes and marks the response uncacheable past the limit.

use bytes::Bytes;
use parking_lot::RwLock;
use async_trait::async_trait;
use pingora_cache::key::{CacheHashKey, CacheKey, CompactCacheKey};
use pingora_cache::storage::{
    HandleHit, HandleMiss, HitHandler, MissFinishType, MissHandler, PurgeType, Storage,
    streaming_write::U64WriteId,
};
use pingora_cache::trace::SpanHandle;
use pingora_cache::CacheMeta;
use pingora_error::{Error, ErrorType, Result};
use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use tokio::sync::watch;

type BinaryMeta = (Vec<u8>, Vec<u8>);

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
    body: Arc<Vec<u8>>,
}

/// A write in progress. Readers attach to this and follow it.
struct Temp {
    meta: BinaryMeta,
    body: Arc<RwLock<Vec<u8>>>,
    written: Arc<watch::Sender<Written>>,
}

impl Temp {
    fn new(meta: BinaryMeta) -> Self {
        let (tx, _rx) = watch::channel(Written::Partial(0));
        Temp { meta, body: Arc::new(RwLock::new(Vec::new())), written: Arc::new(tx) }
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
    used: AtomicUsize,
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
            used: AtomicUsize::new(0),
            max_bytes,
            next_id: AtomicU64::new(0),
        }))
    }

    pub fn bytes_used(&self) -> usize {
        self.used.load(Ordering::Relaxed)
    }

    pub fn entries(&self) -> usize {
        self.cached.read().len()
    }

    fn admit(&self, key: String, obj: Stored) {
        let size = obj.body.len() + obj.meta.0.len() + obj.meta.1.len();
        if size > self.max_bytes {
            return; // oversized entries are streamed to the client, never stored
        }
        {
            let mut cached = self.cached.write();
            let mut order = self.order.write();
            while self.used.load(Ordering::Relaxed) + size > self.max_bytes {
                let Some(victim) = order.pop_front() else { break };
                if let Some(v) = cached.remove(&victim) {
                    let freed = v.body.len() + v.meta.0.len() + v.meta.1.len();
                    self.used.fetch_sub(freed, Ordering::Relaxed);
                }
            }
            if let Some(old) = cached.insert(key.clone(), obj) {
                let freed = old.body.len() + old.meta.0.len() + old.meta.1.len();
                self.used.fetch_sub(freed, Ordering::Relaxed);
                order.retain(|k| k != &key);
            }
            order.push_back(key);
        }
        self.used.fetch_add(size, Ordering::Relaxed);
    }
}

#[async_trait]
impl Storage for BoundedStore {
    async fn lookup(&'static self, key: &CacheKey, _t: &SpanHandle) -> Result<Option<(CacheMeta, HitHandler)>> {
        let hash = key.combined();

        if let Some(obj) = self.cached.read().get(&hash).cloned() {
            let meta = CacheMeta::deserialize(&obj.meta.0, &obj.meta.1)?;
            return Ok(Some((meta, Box::new(CompleteHit { body: obj.body, read: 0, done: false }))));
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
                writes.iter().max_by_key(|(id, _)| **id).map(|(_, t)| {
                    (t.meta.clone(), t.body.clone(), t.written.subscribe())
                })
            })
        };
        let Some((meta, body, written)) = partial else {
            return Ok(None);
        };
        let meta = CacheMeta::deserialize(&meta.0, &meta.1)?;
        Ok(Some((meta, Box::new(PartialHit { body, written, read: 0 }))))
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
                (temp.meta.clone(), temp.body.clone(), temp.written.subscribe())
            })
        };
        let Some((meta, body, written)) = found else {
            // The write finished (or failed) between the lock release and this
            // lookup. Falling back to the completed entry is correct, and not
            // panicking is the difference between test-only and production.
            return self.lookup(key, t).await;
        };
        let meta = CacheMeta::deserialize(&meta.0, &meta.1)?;
        Ok(Some((meta, Box::new(PartialHit { body, written, read: 0 }))))
    }

    async fn get_miss_handler(
        &'static self,
        key: &CacheKey,
        meta: &CacheMeta,
        _t: &SpanHandle,
    ) -> Result<MissHandler> {
        let hash = key.combined();
        let temp = Temp::new(meta.serialize()?);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let handler = FollowableMiss {
            store: self,
            key: hash.clone(),
            id: id.into(),
            body: temp.body.clone(),
            written: temp.written.clone(),
        };
        self.temp.write().entry(hash).or_default().insert(id, temp);
        Ok(Box::new(handler))
    }

    async fn purge(&'static self, key: &CompactCacheKey, _p: PurgeType, _t: &SpanHandle) -> Result<bool> {
        let hash = key.combined();
        let removed = self.cached.write().remove(&hash);
        if let Some(v) = &removed {
            self.used.fetch_sub(v.body.len(), Ordering::Relaxed);
            self.order.write().retain(|k| k != &hash);
        }
        Ok(removed.is_some())
    }

    async fn update_meta(&'static self, key: &CacheKey, meta: &CacheMeta, _t: &SpanHandle) -> Result<bool> {
        let hash = key.combined();
        let mut cached = self.cached.write();
        let Some(obj) = cached.get_mut(&hash) else { return Ok(false) };
        obj.meta = meta.serialize()?;
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
    body: Arc<Vec<u8>>,
    read: usize,
    done: bool,
}

#[async_trait]
impl HandleHit for CompleteHit {
    async fn read_body(&mut self) -> Result<Option<Bytes>> {
        if self.done {
            return Ok(None);
        }
        self.done = true;
        Ok(Some(Bytes::copy_from_slice(&self.body[self.read..])))
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
        self.read = start.min(self.body.len());
        self.done = false;
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
                let chunk = Bytes::copy_from_slice(&self.body.read()[self.read..available]);
                self.read = available;
                return Ok(Some(chunk));
            }
            if state.done() {
                return Ok(None);
            }
            if self.written.changed().await.is_err() {
                // The writer vanished without finishing.
                return Err(Error::explain(ErrorType::InternalError, "cache writer dropped"));
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
}

#[async_trait]
impl HandleMiss for FollowableMiss {
    async fn write_body(&mut self, data: Bytes, eof: bool) -> Result<()> {
        let so_far = self.written.borrow().bytes();
        self.body.write().extend_from_slice(&data);
        let total = so_far + data.len();
        self.written
            .send_replace(if eof { Written::Complete(total) } else { Written::Partial(total) });
        Ok(())
    }

    async fn finish(self: Box<Self>) -> Result<MissFinishType> {
        let id: u64 = self.id.into();
        let temp = self.store.temp.write().get_mut(&self.key).and_then(|m| m.remove(&id));
        let Some(temp) = temp else {
            return Err(Error::explain(ErrorType::InternalError, "write vanished"));
        };
        // Make sure any follower that has not drained yet sees the end.
        let total = self.written.borrow().bytes();
        self.written.send_replace(Written::Complete(total));

        let body = Arc::new(temp.body.read().clone());
        let size = body.len();
        self.store.admit(self.key.clone(), Stored { meta: temp.meta, body });
        Ok(MissFinishType::Created(size))
    }

    fn streaming_write_tag(&self) -> Option<&[u8]> {
        Some(self.id.as_bytes())
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
        leader.write_body(Bytes::from_static(b"shell"), false).await.unwrap();

        let hit = store.lookup(&key, &Span::inactive().handle()).await.unwrap();
        let (_meta, mut reader) = hit.expect("a waiter must be able to attach to the in-flight write");
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
        first.write_body(Bytes::from_static(b"old"), true).await.unwrap();
        first.finish().await.unwrap();

        // A revalidation starts while the finished entry is still servable.
        let mut second = store
            .get_miss_handler(&key, &meta(), &Span::inactive().handle())
            .await
            .unwrap();
        second.write_body(Bytes::from_static(b"new"), false).await.unwrap();

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
        assert!(store.lookup(&key, &Span::inactive().handle()).await.unwrap().is_none());
    }
}
