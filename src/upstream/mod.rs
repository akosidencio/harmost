//! Where a request goes, and whether that backend is worth sending it to.

pub mod health;

use std::net::{SocketAddr, ToSocketAddrs};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::config::schema::LoadBalancing;

#[derive(Debug, Clone)]
pub struct Backend {
    pub id: usize,
    pub address: String,
    pub socket: SocketAddr,
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
    pub fn new(addresses: &[String], strategy: LoadBalancing) -> Result<Self, String> {
        let backends = addresses
            .iter()
            .enumerate()
            .map(|(id, address)| {
                let socket = address
                    .to_socket_addrs()
                    .map_err(|error| format!("could not resolve upstream `{address}`: {error}"))?
                    .next()
                    .ok_or_else(|| format!("upstream `{address}` resolved to no addresses"))?;
                Ok(Backend {
                    id,
                    address: address.clone(),
                    socket,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let healthy = backends
            .iter()
            .map(|_| Arc::new(std::sync::atomic::AtomicBool::new(true)))
            .collect();
        Ok(UpstreamPool {
            backends,
            strategy,
            cursor: AtomicUsize::new(0),
            healthy,
        })
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
        self.healthy
            .get(id)
            .is_some_and(|h| h.load(Ordering::Relaxed))
    }

    /// Pick a backend for this path.
    pub fn select(&self, path: &str) -> Option<&Backend> {
        if self.backends.is_empty() {
            return None;
        }
        let healthy: Vec<&Backend> = self
            .backends
            .iter()
            .filter(|b| self.is_healthy(b.id))
            .collect();
        // Nothing healthy: serve anyway rather than converting a degraded
        // origin into a guaranteed outage.
        let pool: &[&Backend] = if healthy.is_empty() {
            let len = NonZeroUsize::new(self.backends.len())?;
            return self.backends.get(self.next_index(path, len));
        } else {
            &healthy
        };
        let len = NonZeroUsize::new(pool.len())?;
        pool.get(self.next_index(path, len)).copied()
    }

    /// `len` is a `NonZeroUsize` because both arms divide by it. The emptiness
    /// check lives in `select` above, and taking the precondition in the type
    /// keeps it from drifting away from the division that depends on it.
    fn next_index(&self, path: &str, len: NonZeroUsize) -> usize {
        match self.strategy {
            LoadBalancing::RoundRobin => self.cursor.fetch_add(1, Ordering::Relaxed) % len,
            // Sending a given path to a consistent backend also warms the
            // origin's own render cache and JIT state, which is free origin
            // work avoided on top of anything Harmost does.
            LoadBalancing::HashByPath => {
                // The remainder is smaller than `len`, so it always fits back
                // into a `usize`; `unwrap_or` is unreachable, not a fallback.
                usize::try_from(fnv1a(path.as_bytes()) % len.get() as u64).unwrap_or(0)
            }
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
            &[
                "127.0.0.1:3000".to_string(),
                "127.0.0.2:3000".to_string(),
                "127.0.0.3:3000".to_string(),
            ],
            strategy,
        )
        .unwrap()
    }

    #[test]
    fn round_robin_cycles_through_every_backend() {
        let p = pool(LoadBalancing::RoundRobin);
        let picks: Vec<&str> = (0..6)
            .map(|_| p.select("/x").unwrap().address.as_str())
            .collect();
        assert_eq!(
            picks,
            [
                "127.0.0.1:3000",
                "127.0.0.2:3000",
                "127.0.0.3:3000",
                "127.0.0.1:3000",
                "127.0.0.2:3000",
                "127.0.0.3:3000",
            ]
        );
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
        let ids: std::collections::HashSet<usize> = (0..60)
            .map(|i| p.select(&format!("/p/{i}")).unwrap().id)
            .collect();
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
