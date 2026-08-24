//! Where a request goes, and whether that backend is worth sending it to.

pub mod health;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::config::schema::LoadBalancing;

#[derive(Debug, Clone)]
pub struct Backend {
    pub id: usize,
    pub address: String,
}

pub struct UpstreamPool {
    backends: Vec<Backend>,
    strategy: LoadBalancing,
    cursor: AtomicUsize,
    /// Set false by health checking. A pool with nothing healthy still serves
    /// — refusing to pick would turn a degraded origin into a hard outage,
    /// and stale-if-error exists precisely for this window.
    healthy: Vec<Arc<std::sync::atomic::AtomicBool>>,
}

impl UpstreamPool {
    pub fn new(addresses: &[String], strategy: LoadBalancing) -> Self {
        let backends = addresses
            .iter()
            .enumerate()
            .map(|(id, address)| Backend { id, address: address.clone() })
            .collect::<Vec<_>>();
        let healthy = backends
            .iter()
            .map(|_| Arc::new(std::sync::atomic::AtomicBool::new(true)))
            .collect();
        UpstreamPool { backends, strategy, cursor: AtomicUsize::new(0), healthy }
    }

    pub fn len(&self) -> usize {
        self.backends.len()
    }

    pub fn backends(&self) -> &[Backend] {
        &self.backends
    }

    pub fn is_empty(&self) -> bool {
        self.backends.is_empty()
    }

    pub fn set_healthy(&self, id: usize, healthy: bool) {
        if let Some(flag) = self.healthy.get(id) {
            flag.store(healthy, Ordering::Relaxed);
        }
    }

    fn is_healthy(&self, id: usize) -> bool {
        self.healthy.get(id).is_some_and(|h| h.load(Ordering::Relaxed))
    }

    /// Pick a backend for this path.
    pub fn select(&self, path: &str) -> Option<&Backend> {
        if self.backends.is_empty() {
            return None;
        }
        let healthy: Vec<&Backend> = self.backends.iter().filter(|b| self.is_healthy(b.id)).collect();
        // Nothing healthy: serve anyway rather than converting a degraded
        // origin into a guaranteed outage.
        let pool: &[&Backend] = if healthy.is_empty() {
            return self.backends.get(self.next_index(path, self.backends.len()));
        } else {
            &healthy
        };
        pool.get(self.next_index(path, pool.len())).copied()
    }

    fn next_index(&self, path: &str, len: usize) -> usize {
        match self.strategy {
            LoadBalancing::RoundRobin => self.cursor.fetch_add(1, Ordering::Relaxed) % len,
            // Sending a given path to a consistent backend also warms the
            // origin's own render cache and JIT state, which is free origin
            // work avoided on top of anything Harmost does.
            LoadBalancing::HashByPath => (fnv1a(path.as_bytes()) % len as u64) as usize,
        }
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(strategy: LoadBalancing) -> UpstreamPool {
        UpstreamPool::new(
            &["a:3000".to_string(), "b:3000".to_string(), "c:3000".to_string()],
            strategy,
        )
    }

    #[test]
    fn round_robin_cycles_through_every_backend() {
        let p = pool(LoadBalancing::RoundRobin);
        let picks: Vec<&str> = (0..6).map(|_| p.select("/x").unwrap().address.as_str()).collect();
        assert_eq!(picks, ["a:3000", "b:3000", "c:3000", "a:3000", "b:3000", "c:3000"]);
    }

    #[test]
    fn hash_by_path_is_stable_for_one_path() {
        let p = pool(LoadBalancing::HashByPath);
        let first = p.select("/products/iphone").unwrap().id;
        for _ in 0..20 {
            assert_eq!(p.select("/products/iphone").unwrap().id, first);
        }
    }

    #[test]
    fn hash_by_path_spreads_distinct_paths() {
        let p = pool(LoadBalancing::HashByPath);
        let ids: std::collections::HashSet<usize> =
            (0..60).map(|i| p.select(&format!("/p/{i}")).unwrap().id).collect();
        assert!(ids.len() > 1, "every path landed on one backend");
    }

    #[test]
    fn unhealthy_backends_are_skipped() {
        let p = pool(LoadBalancing::RoundRobin);
        p.set_healthy(0, false);
        for _ in 0..10 {
            assert_ne!(p.select("/x").unwrap().id, 0);
        }
    }

    #[test]
    fn a_fully_unhealthy_pool_still_serves() {
        // Refusing to pick would turn a degraded origin into a hard outage.
        let p = pool(LoadBalancing::RoundRobin);
        for id in 0..3 {
            p.set_healthy(id, false);
        }
        assert!(p.select("/x").is_some());
    }
}
