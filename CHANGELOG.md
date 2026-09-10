# Changelog

Notable changes by version. See the [roadmap](./docs/ROADMAP.md) for remaining
work and [operations guide](./docs/OPERATIONS.md) for deployment details.

## 0.1.3 Unreleased

- Added checked-in Next.js route approvals with strict fail-closed validation.
- Added `inspect`, `doctor`, `explain`, and guarded capacity calibration workflows.
- Added staged `observe`, `protect`, `coalesce`, and `cache` rollout generation.
- Added a production-shaped Next.js reference with shared cache state, stable identity, an edge boundary, and expanded CI checks.
- Added Prometheus recording rules and deployment identity artifacts.
- Added conservative replica-group capacity partitioning, URI-stable ingress, and purge fan-out.
- Added standalone setup with generated configuration, automatic config discovery, and a packaged systemd service.

## 0.1.2 — 2026-09-02

- Added the [Next.js adapter](./packages/harmost-next) for build-based configuration, validation, and cache invalidation, with Node and Bun support.
- Added cache tags, authenticated tag/path purges, and cleanup when the deployment changes.
- Added circuit breakers, bounded retries, load-aware balancing, route priorities, and weighted capacity limits.
- Improved cache eviction and fixed Next.js image caching across negotiated formats.
- Expanded resilience and cache metrics, alerts, benchmarks, and configuration checks.
- Evaluated disk and external cache storage; retained the in-memory cache.
- Upgrade note: cache eviction now defaults to `clock`; set `eviction: fifo` to retain the previous policy. Purges and protection budgets remain per process.

## 0.1.1 — 2026-08-29

- Added HTTP/2, optional TLS, WebSockets, trusted-proxy handling, and optional response spooling for slow clients.
- Added health/status endpoints, graceful draining, and Linux socket-handover restarts.
- Added distributed tracing, configuration versioning, richer metrics, dashboards, and alerts.
- Added soak, memory-pressure, restart, chaos, and security checks, plus release artifact automation.
- Fixed cache isolation across hosts and schemes, forwarded-header spoofing, concurrency resizing, and shutdown/readiness behavior.
- Tightened startup validation, reload checks, and build safeguards.
- Deployment notes: configure readiness and per-instance restart paths, allow enough shutdown time, and use a local collector for plaintext-only trace export. Native TLS remains experimental.

## 0.1.0 — 2026-08-27

- Introduced origin concurrency limits, bounded queues, and overload shedding.
- Added bounded in-memory caching, streaming request coalescing, and stale-response support.
- Added upstream health checks, load balancing, safe configuration reloads, metrics, and access logs.
- Added Next.js integration fixtures, browser checks, benchmarks, property tests, and fuzzing.
- Fixed cache-key collisions, private-response isolation, stale revalidation, and streaming coalescing.
