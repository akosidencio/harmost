# Roadmap

Harmost protects expensive SSR and dynamic origin workloads. It remains a
working prototype without sustained production validation.

## 5. Complete the Next.js production reference

- Validate the [production reference](./NEXTJS-PRODUCTION-REFERENCE.md) in a production-shaped staging environment.
- Complete an independent [cache-safety review](./CACHE-KEY-REVIEW.md), published-artifact and rollback verification, and a sustained deployment report.

## 6. Validate scaling across Harmost replicas

- Partition one global origin-work budget across replicas and test scaling, failures, and recovery.
- Validate path-stable ingress with multiple Harmost replicas.
- Evaluate capacity leases and adaptive concurrency after fixed limits are proven.
- Revisit distributed caching and coalescing only when measurements justify them.

## 7. Expand framework support

- After phases 5–6, extract a versioned adapter contract and shared conformance tests.
- Choose the next framework based on demonstrated self-hosting demand.

## Current boundaries

- Cache and protection state are local to each process; limits and purges need coordination across replicas.
- Path purges match exact paths; dynamic route-pattern invalidation is not implemented.
- Slow readers can hold origin capacity unless response spooling is enabled, which sacrifices progressive rendering.
- Disk and external cache storage were [evaluated and declined](./CACHE-STORAGE-EVALUATION.md).
- Harmost does not replace an edge server, CDN, authentication, or client rate limiting.

See the [changelog](../CHANGELOG.md) for release history, the
[Next.js production reference](./NEXTJS-PRODUCTION-REFERENCE.md), and
[operations](./OPERATIONS.md) and [release gates](./RELEASE-GATES.md) for
deployment requirements.
