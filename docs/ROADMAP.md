# Roadmap

Harmost protects expensive SSR and dynamic origin workloads. It remains a
working prototype without sustained production validation.

## 6. Complete replica validation

- Validate the static partition and path-stable reference in production-shaped staging.
- Revisit capacity leases, adaptive limits, and distributed reuse only if fixed partitions prove insufficient.

## 7. Complete standalone distribution

- Validate the binary, generated config, systemd unit, and domain guide on a clean server.
- Add Linux ARM64 and package-manager installation when demand justifies them.

## 8. Expand framework support

- After phases 5–6, extract a versioned adapter contract and shared conformance tests.
- Choose the next framework based on demonstrated self-hosting demand.

## Current boundaries

- Cache and coalescing remain local to each process; replica limits are statically partitioned and purges must reach every admin endpoint.
- Path purges match exact paths; dynamic route-pattern invalidation is not implemented.
- Slow readers can hold origin capacity unless response spooling is enabled, which sacrifices progressive rendering.
- Disk and external cache storage were [evaluated and declined](./CACHE-STORAGE-EVALUATION.md).
- Harmost does not replace an edge server, CDN, authentication, or client rate limiting.

See the [changelog](../CHANGELOG.md) for release history, the
[Next.js production reference](./NEXTJS-PRODUCTION-REFERENCE.md), and
[operations](./OPERATIONS.md) and [release gates](./RELEASE-GATES.md) for
deployment requirements.
