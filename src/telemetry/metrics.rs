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

// Every `expect` below is a `register_*!` inside a `LazyLock`. A failure means
// a duplicate metric name or a malformed label set — a mistake in this file,
// caught the first time the binary touches the metric, and with no runtime
// recovery worth writing. `expect` rather than `allow`: if these ever stop
// being the only panicking accessors here, the attribute itself goes stale
// and says so.
#![expect(
    clippy::expect_used,
    reason = "metric registration failure is a bug in this file, not a runtime condition"
)]

use prometheus::{
    HistogramVec, IntCounterVec, IntGauge, IntGaugeVec, register_histogram_vec,
    register_int_counter_vec, register_int_gauge, register_int_gauge_vec,
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
        vec![
            0.005, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0
        ]
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

pub static SPOOL: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "harmost_spool_total",
        "Responses that were spooled, by route and outcome",
        &["route", "reason"]
    )
    .expect("metric registration")
});

pub static SPOOL_BYTES: LazyLock<IntGauge> = LazyLock::new(|| {
    register_int_gauge!(
        "harmost_spool_bytes",
        "Bytes currently held across every in-flight response spool"
    )
    .expect("metric registration")
});

pub static CACHE_BYTES: LazyLock<IntGauge> = LazyLock::new(|| {
    register_int_gauge!(
        "harmost_cache_bytes",
        "Bytes held by the response cache, including fills in progress"
    )
    .expect("metric registration")
});

/// The configured cache budget.
///
/// Published so `harmost_cache_bytes` has a denominator. Occupancy on its own
/// is a number nobody can act on: "512MB used" is healthy or an emergency
/// depending entirely on the ceiling, and an alert that hardcodes the ceiling
/// goes stale the first time someone edits the config.
pub static CACHE_MAX_BYTES: LazyLock<IntGauge> = LazyLock::new(|| {
    register_int_gauge!(
        "harmost_cache_max_bytes",
        "Configured cache.max_memory, the ceiling harmost_cache_bytes is measured against"
    )
    .expect("metric registration")
});

/// The configured spool budget, for the same reason.
pub static SPOOL_MAX_BYTES: LazyLock<IntGauge> = LazyLock::new(|| {
    register_int_gauge!(
        "harmost_spool_max_bytes",
        "Configured spool.max_memory, the ceiling harmost_spool_bytes is measured against"
    )
    .expect("metric registration")
});

pub static CACHE_ENTRIES: LazyLock<IntGauge> = LazyLock::new(|| {
    register_int_gauge!(
        "harmost_cache_entries",
        "Completed entries in the response cache"
    )
    .expect("metric registration")
});

pub static UPGRADES: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "harmost_upgrade_total",
        "Protocol upgrade requests, by route and decision",
        &["route", "decision"]
    )
    .expect("metric registration")
});

pub static UPGRADES_ACTIVE: LazyLock<IntGauge> = LazyLock::new(|| {
    register_int_gauge!(
        "harmost_upgrade_active",
        "Upgraded connections currently held open"
    )
    .expect("metric registration")
});

/// Span lifecycle: `recorded`, `dropped` (a full queue), `exported`,
/// `export_failed`. Bounded: the four outcomes in [`super::otlp`].
///
/// `dropped` is the one to alert on. It is the signal that the tracing queue
/// is too small for the traffic, and it says so without the export path having
/// to slow a single request down to tell you.
pub static SPANS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec!(
        "harmost_spans_total",
        "Spans by outcome: recorded, dropped, exported, export_failed",
        &["outcome"]
    )
    .expect("metric registration")
});

/// 1 while the process is draining, 0 otherwise.
///
/// A gauge rather than a counter because the question an operator asks during
/// a deploy is "is this instance still taking traffic", which is a state.
pub static DRAINING: LazyLock<IntGauge> = LazyLock::new(|| {
    register_int_gauge!(
        "harmost_draining",
        "1 while this instance is draining and reporting itself not ready"
    )
    .expect("metric registration")
});

/// The configuration generation currently in force. Increments on every
/// accepted `SIGHUP`; a reload that was refused leaves it unchanged, which is
/// what makes "did my config actually apply" answerable from a dashboard.
pub static CONFIG_GENERATION: LazyLock<IntGauge> = LazyLock::new(|| {
    register_int_gauge!(
        "harmost_config_generation",
        "Generation of the configuration currently in force"
    )
    .expect("metric registration")
});

/// Stable fingerprint of the effective configuration. Unlike generation, this
/// is comparable across replicas that have restarted or reloaded a different
/// number of times.
pub static CONFIG_FINGERPRINT: LazyLock<IntGauge> = LazyLock::new(|| {
    register_int_gauge!(
        "harmost_config_fingerprint",
        "Stable fingerprint of the effective configuration currently in force"
    )
    .expect("metric registration")
});

/// 1 per healthy upstream, 0 per unhealthy one. Labelled by the configured
/// address, which is config-derived like every other label in this file.
pub static UPSTREAM_HEALTHY: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    register_int_gauge_vec!(
        "harmost_upstream_healthy",
        "1 when a backend is passing its health check, 0 when it is not",
        &["upstream"]
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
    LazyLock::force(&SPOOL);
    LazyLock::force(&SPOOL_BYTES);
    LazyLock::force(&CACHE_BYTES);
    LazyLock::force(&CACHE_MAX_BYTES);
    LazyLock::force(&SPOOL_MAX_BYTES);
    LazyLock::force(&CACHE_ENTRIES);
    LazyLock::force(&UPGRADES);
    LazyLock::force(&UPGRADES_ACTIVE);
    LazyLock::force(&SPANS);
    LazyLock::force(&DRAINING);
    LazyLock::force(&CONFIG_GENERATION);
    LazyLock::force(&CONFIG_FINGERPRINT);
    LazyLock::force(&UPSTREAM_HEALTHY);
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
        let allowed = [
            "route", "class", "status", "reason", "decision", "upstream", "limiter", "outcome",
        ];
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
