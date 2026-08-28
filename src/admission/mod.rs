//! Bounding how much origin work is in flight.
//!
//! This is the part of Harmost that keeps working when nothing is cacheable and
//! nothing can be collapsed, which is most of what a dynamic SSR app looks like.
//!
//! Order matters and is deliberate: reuse opportunities are exhausted *before*
//! admission, because a cache hit and a coalescing waiter consume no origin
//! capacity and should never be made to queue for it.

pub mod limiter;

use crate::classifier::RequestClass;
use limiter::{Limiter, Permit, ShedReason};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

/// The outcome of asking for origin capacity.
#[derive(Debug)]
pub enum Admission {
    /// Hold these for exactly as long as the origin is working.
    Admitted(Permits),
    /// This class does not consume origin capacity. Static assets are served
    /// without rendering; streaming responses would hold a permit for the life
    /// of the connection and starve everything else.
    Exempt,
    Shed(ShedReason),
}

/// A request needs room globally *and* on its route. Both are released
/// together when this drops.
#[derive(Debug)]
pub struct Permits {
    route: Option<Permit>,
    global: Permit,
}

impl Permits {
    /// Collapse both permits into one value the request can own. The route
    /// permit rides along so that dropping this releases both together.
    pub fn into_inner(self) -> Permit {
        // Keep the route permit alive for exactly as long as the global one.
        self.global.with_companion(self.route)
    }
}

pub struct AdmissionController {
    global: Arc<Limiter>,
    /// Keyed by route id and deliberately outliving any one config
    /// generation — a limiter carries in-flight state that a policy swap
    /// must not discard.
    ///
    /// `parking_lot` rather than `std`: this lock is taken on the request
    /// path, and a poisoned `std` lock would leave every later request with
    /// nothing to do but panic again.
    routes: RwLock<HashMap<String, Arc<Limiter>>>,
}

impl AdmissionController {
    pub fn new(global_max: usize, queue_max: usize, queue_timeout: Duration) -> Self {
        AdmissionController {
            global: Limiter::new("global", global_max, queue_max, queue_timeout),
            routes: RwLock::new(HashMap::new()),
        }
    }

    pub fn global(&self) -> &Arc<Limiter> {
        &self.global
    }

    /// Get or create the limiter for a route. Created on first use so that a
    /// newly added route does not need a restart.
    pub fn route_limiter(
        &self,
        id: &str,
        max: usize,
        queue_max: usize,
        queue_timeout: Duration,
    ) -> Arc<Limiter> {
        if let Some(l) = self.routes.read().get(id) {
            return l.clone();
        }
        let mut w = self.routes.write();
        w.entry(id.to_string())
            .or_insert_with(|| Limiter::new(id, max, queue_max, queue_timeout))
            .clone()
    }

    /// Every route limiter currently registered, for the admin status
    /// document. A snapshot of the `Arc`s rather than a borrow, so the read
    /// lock is held for the length of a clone and not for the length of
    /// rendering a JSON document.
    pub fn route_limiters(&self) -> Vec<Arc<Limiter>> {
        let mut limiters: Vec<Arc<Limiter>> = self.routes.read().values().cloned().collect();
        // Stable order: a status document whose fields move between scrapes
        // is one nobody can diff.
        limiters.sort_by(|a, b| a.name().cmp(b.name()));
        limiters
    }

    /// Apply new limits from a reloaded config, in place.
    ///
    /// Limiters are resized rather than replaced, and one that has disappeared
    /// from the config is dropped from the registry but stays alive until its
    /// last in-flight permit returns.
    pub fn apply_limits(
        &self,
        global_max: usize,
        global_queue_max: usize,
        global_queue_timeout: Duration,
        routes: &[(String, usize, usize, Duration)],
    ) {
        self.global.resize(global_max);
        self.global
            .set_queue(global_queue_max, global_queue_timeout);
        let mut w = self.routes.write();
        for (id, max, q_max, q_timeout) in routes {
            match w.get(id) {
                Some(existing) => {
                    existing.resize(*max);
                    existing.set_queue(*q_max, *q_timeout);
                }
                None => {
                    w.insert(
                        id.clone(),
                        Limiter::new(id.clone(), *max, *q_max, *q_timeout),
                    );
                }
            }
        }
        let keep: Vec<String> = routes.iter().map(|(id, ..)| id.clone()).collect();
        w.retain(|id, _| keep.contains(id));
    }

    /// Ask for capacity.
    ///
    /// The route limiter is taken first: it is the narrower of the two, so
    /// failing there costs nothing globally. Both acquisitions share one
    /// deadline, so a request cannot spend each limiter's queue timeout in
    /// turn and wait twice as long as configured.
    pub async fn admit(&self, class: RequestClass, route: Option<&Arc<Limiter>>) -> Admission {
        if !class.consumes_origin_permit() {
            return Admission::Exempt;
        }

        let budget = std::iter::once(self.global.queue_timeout())
            .chain(route.map(|limiter| limiter.queue_timeout()))
            .filter(|duration| !duration.is_zero())
            .min();
        let deadline = budget.map(|duration| Instant::now() + duration);

        let route_permit = match route {
            Some(r) => match r.acquire(deadline).await {
                Ok(p) => Some(p),
                Err(reason) => return Admission::Shed(reason),
            },
            None => None,
        };

        match self.global.acquire(deadline).await {
            Ok(g) => Admission::Admitted(Permits {
                route: route_permit,
                global: g,
            }),
            // Dropping `route_permit` here hands the route slot straight back.
            Err(reason) => Admission::Shed(reason),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn controller(global: usize) -> AdmissionController {
        AdmissionController::new(global, 0, Duration::ZERO)
    }

    #[tokio::test]
    async fn static_and_streaming_bypass_the_permit_entirely() {
        let c = controller(1);
        let _held = c.admit(RequestClass::PublicDocument, None).await;
        // The one global permit is gone, yet these still pass.
        assert!(matches!(
            c.admit(RequestClass::Static, None).await,
            Admission::Exempt
        ));
        assert!(matches!(
            c.admit(RequestClass::Streaming, None).await,
            Admission::Exempt
        ));
    }

    #[tokio::test]
    async fn admission_requires_both_route_and_global_capacity() {
        let c = controller(10);
        let route = c.route_limiter("search", 1, 0, Duration::ZERO);
        let _a = c.admit(RequestClass::PublicDynamic, Some(&route)).await;
        // Global has room; the route does not.
        assert!(matches!(
            c.admit(RequestClass::PublicDynamic, Some(&route)).await,
            Admission::Shed(_)
        ));
    }

    #[tokio::test]
    async fn a_route_slot_is_returned_when_the_global_limit_refuses() {
        let c = controller(1);
        let route = c.route_limiter("r", 5, 0, Duration::ZERO);
        let _global_hog = c.admit(RequestClass::PublicDocument, None).await;

        assert!(matches!(
            c.admit(RequestClass::PublicDocument, Some(&route)).await,
            Admission::Shed(_)
        ));
        // The route limiter must not have leaked the slot it briefly held.
        assert_eq!(
            route.available(),
            5,
            "route permit was not released after global shed"
        );
    }

    #[tokio::test]
    async fn origin_concurrency_never_exceeds_the_configured_maximum() {
        // The acceptance criterion from the spec, as an executable test.
        const MAX: usize = 10;
        const ARRIVALS: usize = 200;

        let c = Arc::new(AdmissionController::new(MAX, 500, Duration::from_secs(5)));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let mut tasks = Vec::new();
        for _ in 0..ARRIVALS {
            let (c, in_flight, peak) = (c.clone(), in_flight.clone(), peak.clone());
            tasks.push(tokio::spawn(async move {
                if let Admission::Admitted(p) = c.admit(RequestClass::PublicDocument, None).await {
                    let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    drop(p);
                }
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }

        assert!(
            peak.load(Ordering::SeqCst) <= MAX,
            "peak {} exceeded {MAX}",
            peak.load(Ordering::SeqCst)
        );
        assert_eq!(in_flight.load(Ordering::SeqCst), 0, "a permit leaked");
    }

    #[tokio::test]
    async fn reload_resizes_in_place_and_keeps_the_same_limiter() {
        let c = controller(10);
        let before = c.route_limiter("products", 100, 0, Duration::ZERO);
        c.apply_limits(
            20,
            25,
            Duration::from_secs(2),
            &[("products".into(), 50, 10, Duration::from_secs(1))],
        );
        let after = c.route_limiter("products", 100, 0, Duration::ZERO);

        assert!(
            Arc::ptr_eq(&before, &after),
            "reload replaced the limiter instead of resizing it"
        );
        assert_eq!(after.limit(), 50);
        assert_eq!(c.global().limit(), 20);
        assert_eq!(c.global().queue_timeout(), Duration::from_secs(2));
    }

    #[tokio::test]
    async fn a_route_removed_from_config_leaves_the_registry() {
        let c = controller(10);
        c.route_limiter("old", 5, 0, Duration::ZERO);
        c.apply_limits(
            10,
            0,
            Duration::ZERO,
            &[("new".into(), 5, 0, Duration::ZERO)],
        );
        assert!(c.routes.read().get("old").is_none());
        assert!(c.routes.read().get("new").is_some());
    }

    #[tokio::test]
    async fn the_shorter_global_queue_deadline_wins_over_a_route_deadline() {
        let c = AdmissionController::new(1, 10, Duration::from_millis(30));
        let route = c.route_limiter("slow-route", 10, 10, Duration::from_secs(1));
        let _held = c.admit(RequestClass::PublicDocument, None).await;
        let started = Instant::now();

        assert!(matches!(
            c.admit(RequestClass::PublicDocument, Some(&route)).await,
            Admission::Shed(ShedReason::QueueTimeout)
        ));
        assert!(started.elapsed() < Duration::from_millis(200));
    }
}
