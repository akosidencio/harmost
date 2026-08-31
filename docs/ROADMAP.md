# Roadmap

This file tracks active work only. Completed features and fixes belong in the
[`CHANGELOG.md`](../CHANGELOG.md), while the evidence required for a release is
defined in [`RELEASE-GATES.md`](./RELEASE-GATES.md).

Harmost remains a working prototype with no production validation. The core
proxy, admission, cache, coalescing, protocol, observability, and restart paths
are implemented and tested against local fixtures. The most important open
validation work is an independent review of cache keys and response
shareability; the review brief is in
[`CACHE-KEY-REVIEW.md`](./CACHE-KEY-REVIEW.md).

## Current limitations and non-goals

- Without `spool.enabled`, a slow reader can occupy a render slot until
  `timeouts.downstream_write`. Spooling avoids that at the cost of progressive
  rendering.
- Cache, coalescing, and admission state are process-local. Replicas multiply
  origin ceilings unless operators partition their limits, and the same key
  may render once per replica.
- The cache is bounded in-process memory with FIFO eviction. Restarting clears
  it, and there is no purge API.
- Harmost does not rate-limit bytes, connections, clients, or requests per
  second. Keep an edge component in front of public deployments.
- A direct `SIGTERM` takes at least `drain_period + shutdown_timeout`, including
  when the process is idle.
- OTLP export is plaintext-only. Use a local OpenTelemetry Collector when
  transport security is required.
- Native TLS uses Pingora's experimental rustls backend. External TLS
  termination remains recommended, and `origin.tls.ca` is not supported.
- Several process-wide settings cannot be reloaded. Harmost refuses a reload
  that changes them instead of silently ignoring it.
- No release artifact has yet exercised the documented checksums, SBOM,
  provenance, and reproducible-build workflow end to end.
- Harmost governs SSR origin work; it is not a general-purpose edge server or
  a replacement for client rate limiting, authentication, redirects, or static
  file serving.

See [`OPERATIONS.md`](./OPERATIONS.md) for deployment consequences and
[`THREAT-MODEL.md`](./THREAT-MODEL.md) for intentionally undefended threats.
Milestone numbering continues phases 0–2, whose completed work is recorded in
the changelog.

## 3. Improve origin resilience

- Add passive failure observation, per-backend circuit breakers, and outlier
  ejection alongside active health checks.
- Add bounded retry budgets for eligible idempotent requests only.
- Add least-loaded selection based on in-flight work and latency.
- Add weighted admission, route priorities, and reserved capacity.

## 4. Complete the cache lifecycle and framework integration

- Add a purge API and cache tags, including deployment-safe invalidation and a
  path from Next.js `revalidateTag()` and `revalidatePath()` events.
- Replace FIFO eviction with a measured production policy and evaluate optional
  disk or external storage.
- Build a versioned `@harmost/next` integration for route hints, deployment ids,
  invalidation events, and a tested compatibility matrix.
- Define a versioned, framework-neutral adapter contract for route cost,
  request variants, privacy, deployment ids, and invalidation events.
- Add adapters for other server-rendered applications after that contract is
  stable. Likely candidates include Nuxt, SvelteKit, React Router/Remix, Astro
  SSR, and non-JavaScript origins that can provide equivalent metadata.

## 5. Scale deliberately

- Evaluate adaptive concurrency only after latency and failure signals are
  trustworthy; retain operator-defined hard ceilings.
- Define a multi-instance capacity model that cannot accidentally multiply the
  intended origin ceiling.
- Keep distributed coalescing optional. Prefer path-stable ingress routing
  until measurements justify a distributed lock.
