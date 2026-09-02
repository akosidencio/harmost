# Roadmap

This file tracks active work only. Completed features and fixes belong in the
[`CHANGELOG.md`](../CHANGELOG.md), while the evidence required for a release is
defined in [`RELEASE-GATES.md`](./RELEASE-GATES.md).

Harmost remains a working prototype with no sustained production validation.
It is not a general Next.js performance accelerator; it is an overload governor
for expensive SSR and dynamic origin workloads. The core proxy, admission,
cache, coalescing, protocol, observability, and restart paths are implemented
and tested against local fixtures. The most important open validation work is
an independent review of cache keys and response shareability; the review
brief is in
[`CACHE-KEY-REVIEW.md`](./CACHE-KEY-REVIEW.md).

## Current limitations and non-goals

- Without `spool.enabled`, a slow reader can occupy a render slot until
  `timeouts.downstream_write`. Spooling avoids that at the cost of progressive
  rendering.
- Cache, coalescing, admission, circuit-breaker, and retry-budget state are all
  process-local. Replicas multiply origin ceilings and retry budgets unless
  operators partition their limits, the same key may render once per replica,
  and each replica must observe a failing backend for itself before it ejects
  it.
- Priority tiers isolate ceilings, not queues. Waiting for a permit is FIFO
  within a tier, so a high-priority request that arrives behind a queue of its
  own peers waits its turn.
- Passive failure observation counts any 5xx as an origin failure. An origin
  that answers 5xx for reasons of its own — a deliberate error page, a route
  that 500s on bad input — will trip breakers that no backend deserves.
- The cache is bounded in-process memory. Restarting clears it, and it is not
  shared between replicas — every instance holds its own entries and must be
  purged separately. Disk and external storage were evaluated and declined; see
  [`CACHE-STORAGE-EVALUATION.md`](./CACHE-STORAGE-EVALUATION.md) for the
  measurements and for what would reopen it.
- Purging by path is exact. There is no equivalent of
  `revalidatePath('/products/[slug]', 'page')`, because matching a dynamic
  route pattern needs the route metadata phase 6 introduces.
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
- Harmost governs expensive SSR and dynamic origin work. It is not a general
  Next.js performance accelerator, general-purpose edge server, or replacement
  for client rate limiting, authentication, redirects, static file serving, a
  CDN, or an ordinary reverse proxy.

See [`OPERATIONS.md`](./OPERATIONS.md) for deployment consequences and
[`THREAT-MODEL.md`](./THREAT-MODEL.md) for intentionally undefended threats.
Milestone numbering continues phases 0–4, whose completed work is recorded in
the changelog. Phase 4 closed with a decision rather than a feature on one of
its bullets: disk and external cache storage were evaluated and declined, with
the measurements and the conditions that would reopen the question in
[`CACHE-STORAGE-EVALUATION.md`](./CACHE-STORAGE-EVALUATION.md).

## 5. Scale deliberately

- Evaluate adaptive concurrency only after latency and failure signals are
  trustworthy; retain operator-defined hard ceilings.
- Define a multi-instance capacity model that cannot accidentally multiply the
  intended origin ceiling.
- Keep distributed coalescing optional. Prefer path-stable ingress routing
  until measurements justify a distributed lock.

## 6. Framework integration and production-ready examples

Deliberately after phase 4. An adapter contract designed before the cache
lifecycle it has to drive would be a guess; designed after, it is a description
of something that already works.

The first adapter has shipped: [`@harmost/next`](../packages/harmost-next)
generates route configuration and a deployment id from a Next.js build, and
routes `revalidateTag()` / `revalidatePath()` to the phase 4 purge API. What
remains here is the generalisation — a contract other frameworks can implement
— and the examples.

- Ship working, production-shaped Next.js examples rather than fixtures: a real
  deployment topology, a deployment-id rollover, and an invalidation flow
  someone can copy instead of infer.
- Define a versioned, framework-neutral adapter contract for route cost,
  request variants, privacy, deployment ids, and invalidation events.
- Add adapters for other server-rendered applications after that contract is
  stable. Likely candidates include Nuxt, SvelteKit, React Router/Remix, Astro
  SSR, and non-JavaScript origins that can provide equivalent metadata.
