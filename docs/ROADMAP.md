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

The next milestones are deliberately Next.js-first. Their interfaces, safety
properties, reference topologies, and exit criteria are specified in
[`NEXTJS-PRODUCTION-REFERENCE-SPEC.md`](./NEXTJS-PRODUCTION-REFERENCE-SPEC.md).
No adapter for another framework begins until that specification's exit
criteria pass. A framework-neutral contract should describe a proven
integration rather than predict what one might need.

## 5. Make the Next.js path production-shaped

The first adapter has shipped: [`@harmost/next`](../packages/harmost-next)
generates conservative route configuration and a deployment id from a Next.js
build, and routes `revalidateTag()` / `revalidatePath()` to the purge API. The
current storefront remains an integration fixture, not yet a copyable
production reference.

- Promote the fixture into a documented standalone-container reference with an
  edge/load-balancer boundary, private origin network, health checks, streaming,
  deployment rollover, and scoped invalidation.
- Coordinate Next.js cache/tag state across origins and use consistent build,
  deployment, and Server Action identities. Harmost invalidation must prove that
  Next.js state converges before its own response copy is purged.
- Add a checked-in `harmost.next.yaml` assertion file so operators approve
  public dynamic routes by their Next.js names while generated output remains
  reproducible and private by default.
- Add `inspect`, `doctor`, and `explain` workflows that expose route decisions,
  deployment mismatches, streaming problems, and cache bypasses without sending
  load or mutating production state.
- Add an explicit, guarded calibration workflow. Stop presenting the generated
  concurrency value as safe until it has been measured for the application's
  renders and hardware.
- Document and test the rollout sequence: observe, protect without reuse,
  coalesce approved routes, enable short caching, then replicate.
- Ship the protection dashboard, recording rules, and alerts required by the
  technical specification.
- Exercise build-id rollover, invalidation, private-response isolation, RSC
  variants, Server Actions, streaming, overload, recovery, and restart in CI
  and a production-shaped staging environment.

## 6. Scale the proven Next.js reference deliberately

- Define one global origin-work budget for a Harmost replica group. Begin with
  validated static partitioning so adding replicas cannot silently multiply the
  intended ceiling.
- Exercise path-stable ingress with two or more Harmost replicas. Correctness
  must not depend on affinity, but cache locality and coalescing should benefit
  from it.
- Keep distributed coalescing and distributed response caching out of the
  critical path until measurements justify their failure modes and complexity.
- Evaluate optional short-lived capacity leases only after static partitioning
  passes scale-up, scale-down, replica-loss, and network-partition tests.
- Evaluate adaptive concurrency only after fixed-limit calibration, origin
  latency, failure, queue, and recovery signals are trustworthy. Retain
  operator-defined hard ceilings.

## 7. Extract the framework contract

Only after phases 5 and 6 meet the exit criteria in the Next.js technical
specification:

- Extract a versioned, framework-neutral adapter contract from the working
  Next.js inputs for route cost, request variants, privacy, deployment ids, and
  invalidation events.
- Publish a conformance suite that verifies fail-closed generation and runtime
  privacy barriers before accepting another adapter.
- Select the next framework from measured self-hosting demand. Possible
  candidates include Nuxt, SvelteKit, React Router/Remix, Astro SSR, and
  non-JavaScript origins that can provide equivalent metadata; this list is not
  a delivery commitment or an implementation order.
