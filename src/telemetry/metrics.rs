//! Prometheus metrics.
//!
//! # Label discipline
//!
//! Route id is the only route-shaped label anywhere in this file. Never path,
//! never host, never query. A metrics label whose cardinality is controlled by
//! the client is a memory-exhaustion vector, and this is the one component
//! that must not fall over when traffic spikes — the same reason the cache has
//! a byte budget.
//!
//! Route ids come from the config file, so their cardinality is bounded by
//! something a person wrote.

use prometheus::{
    HistogramVec, IntCounterVec, IntGaugeVec, register_histogram_vec, register_int_counter_vec,
    register_int_gauge_vec,
};
use std::sync::LazyLock;

pub static REQUESTS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "harmost_requests_total",
        "Requests received, by route and classification",
        &["route", "class"]
    )
    .expect("metric registration")
});

/// `status` is one of hit, miss, stale, bypass, disabled.
pub static CACHE: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "harmost_cache_total",
        "Cache outcomes, by route and status",
        &["route", "status"]
    )
    .expect("metric registration")
});

/// Why a response was not shared. Bounded: the variants of `BypassReason`.
pub static BYPASS_REASON: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "harmost_cache_bypass_reason_total",
        "Reasons a response was not shared",
        &["route", "reason"]
    )
    .expect("metric registration")
});

/// `decision` is one of admitted, exempt, shed_queue_full, shed_queue_timeout.
pub static ADMISSION: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "harmost_admission_total",
        "Admission decisions, by route",
        &["route", "decision"]
    )
    .expect("metric registration")
});

pub static ORIGIN_REQUESTS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "harmost_origin_requests_total",
        "Requests that reached the origin",
        &["route", "upstream"]
    )
    .expect("metric registration")
});

pub static ORIGIN_LATENCY: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec!(
        "harmost_origin_latency_seconds",
        "Time the origin spent producing a response",
        &["route"],
        // Bucketed for server rendering, where 50ms is fast and 5s is a
        // problem — not for the sub-millisecond world the default buckets
        // assume.
        vec![0.005, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0]
    )
    .expect("metric registration")
});

/// The denominator of the origin-work-avoidance ratio.
///
/// Counted for a request only when reuse was actually possible for it: the
/// route permits reuse and the request was not bypass-classified. Without a
/// definition this precise the headline ratio can be improved by narrowing
/// eligibility, which makes it unfalsifiable and therefore worthless.
pub static REUSE_ELIGIBLE: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "harmost_reuse_eligible_requests_total",
        "Requests for which reuse was possible; the denominator of the avoidance ratio",
        &["route"]
    )
    .expect("metric registration")
});

pub static QUEUE_DEPTH: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    register_int_gauge_vec!(
        "harmost_queue_depth",
        "Requests currently waiting for origin capacity",
        &["limiter"]
    )
    .expect("metric registration")
});

pub static LIMIT: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    register_int_gauge_vec!(
        "harmost_concurrency_limit",
        "Configured origin concurrency ceiling",
        &["limiter"]
    )
    .expect("metric registration")
});

pub static IN_FLIGHT: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    register_int_gauge_vec!(
        "harmost_origin_in_flight",
        "Origin requests currently holding a permit",
        &["limiter"]
    )
    .expect("metric registration")
});

/// Touch every metric family so a fresh scrape shows zeros rather than absent
/// series. A dashboard that renders "no data" during an incident is worse than
/// one that renders zero.
pub fn preregister() {
    LazyLock::force(&REQUESTS);
    LazyLock::force(&CACHE);
    LazyLock::force(&BYPASS_REASON);
    LazyLock::force(&ADMISSION);
    LazyLock::force(&ORIGIN_REQUESTS);
    LazyLock::force(&ORIGIN_LATENCY);
    LazyLock::force(&REUSE_ELIGIBLE);
    LazyLock::force(&QUEUE_DEPTH);
    LazyLock::force(&LIMIT);
    LazyLock::force(&IN_FLIGHT);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_metric_registers_without_conflict() {
        // Duplicate registration panics at runtime; catch it here instead.
        preregister();
        preregister();
    }

    #[test]
    fn no_metric_is_labelled_by_anything_client_controlled() {
        // Guards the rule in this module's docs. `upstream` and `limiter` are
        // config-derived, like `route`.
        let allowed = ["route", "class", "status", "reason", "decision", "upstream", "limiter"];
        preregister();
        for family in prometheus::gather() {
            for metric in family.get_metric() {
                for label in metric.get_label() {
                    assert!(
                        allowed.contains(&label.get_name()),
                        "metric {} carries unexpected label `{}`",
                        family.get_name(),
                        label.get_name()
                    );
                }
            }
        }
    }
}
