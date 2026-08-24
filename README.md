# Harmost: Rust Reverse Proxy for SSR Origin Protection

**Stop traffic spikes from becoming render spikes.**

Harmost is an open-source Rust reverse proxy and origin workload governor for
server-rendered applications. It protects Next.js and other SSR origins with
bounded concurrency, request coalescing, safe microcaching, bounded queues, and
load shedding.

Unlike a conventional reverse-proxy cache, Harmost limits how much rendering
work may reach an origin at once. Cache hits and duplicate requests are reused
first; genuine cache misses enter per-route and global admission control before
they can consume origin capacity.

Harmost is built on [Pingora](https://github.com/cloudflare/pingora), the Rust
proxy framework Cloudflare wrote to replace their NGINX fleet and now runs more
than a trillion requests a day on. The parts of a reverse proxy that are
unglamorous and easy to get subtly wrong — connection pooling, HTTP/1 and
HTTP/2 handling, graceful restarts, timeouts, the cache lock — are inherited
from a codebase that has been under that load for years. What Harmost adds on
top is the governor: classification, cache-key and shareability rules, and
bounded origin admission.

> A *harmost* (ἁρμοστής, from ἁρμόζω, "to fit, to keep in proper adjustment") was
> an official posted to hold a system in correct adjustment.

## What Harmost does

- **Protects SSR origins from traffic spikes** with global and per-route
  concurrency limits.
- **Collapses duplicate concurrent renders** so identical requests share one
  in-flight origin response when it is safe to do so.
- **Microcaches public server-rendered pages** with strict response
  shareability checks and route-level TTL ceilings.
- **Bounds overload queues** with explicit queue sizes and deadlines, then
  serves stale content or sheds load instead of overwhelming the origin.
- **Understands Next.js request variants**, including React Server Components,
  prefetch requests, draft mode, Server Actions, and static assets.
- **Keeps private responses private** by treating `Set-Cookie`, authorization,
  unsafe `Vary` headers, and private cache directives as absolute barriers.
- **Balances requests across origins** with round-robin or path-stable hashing.

Typical use cases include protecting Next.js storefronts during product drops,
absorbing bursts on public SSR pages, and enforcing predictable origin capacity
for mixed public, private, static, and dynamic routes.

## Contents

- [What Harmost does](#what-harmost-does)
- [Why use Harmost instead of a standard reverse-proxy cache?](#why-use-harmost-instead-of-a-standard-reverse-proxy-cache)
- [Request coalescing benchmark](#request-coalescing-benchmark)
- [Private-response safety check](#private-response-safety-check)
- [Quick start](#quick-start)
- [Project status](#project-status)
- [Design principles](#design-principles)
- [Request coalescing and microcache architecture](#request-coalescing-and-microcache-architecture)
- [Next.js caching and cache-key design](#nextjs-caching-and-cache-key-design)
- [Configuration](#configuration)
- [License](#license)

## Why use Harmost instead of a standard reverse-proxy cache?

Request collapsing is not new — Varnish, `proxy_cache_lock`, Fastly and Apache
Traffic Server have all done it for years, and forty lines of `nginx.conf` will
collapse a thousand concurrent hits on one URL down to a single origin request.

What none of them do is bound *origin work* per route, with a bounded queue, a
deadline on that queue, and stale-or-shed when it overflows. `limit_conn` caps
connections with no queue deadline and no stale fallback. Envoy has adaptive
concurrency but no notion of a render or a shareable document.

So the demo that matters is not the cacheable one:

```
$ ./bench/demo.sh 60 1000

60 concurrent requests, 60 unique URLs, 1000ms render
nothing cacheable, nothing coalescible — admission control only

  direct to origin       peak=60     wall=1s
  through harmost        peak=10     wall=6s

  configured ceiling     10
  origin peak, direct    60
  origin peak, harmost   10
```

The origin reports its own peak concurrency, so the result does not depend on
trusting the proxy's own metrics.

Read the wall-clock column honestly: harmost made this workload *slower*. Sixty
renders that a healthy origin could absorb at once were served ten at a time.
That is the trade — bounded latency growth instead of an origin driven past the
point where it recovers. The demo fixture sleeps rather than rendering, and a
sleep parallelises perfectly, so it shows the concurrency bound while
understating what the bound buys you: a real renderer at 6× its healthy
concurrency does not degrade linearly.

Caching and coalescing are the bonus. Governing is the product.

## Request coalescing benchmark

```
$ ./bench/coalesce.sh 100 1000

100 concurrent requests for ONE url, 1000ms render

  requests served        100 / 100
  origin renders         1

  X-Harmost breakdown:
      99 HIT
       1 MISS
```

## Private-response safety check

Same route, same permissive config. This path answers with `Set-Cookie`:

```
$ ./bench/safety.sh 50

  requests served        50 / 50
  distinct session ids   50

  X-Harmost breakdown:
      50 MISS

PASS: every request got its own session; nothing was shared
```

Fifty renders where a moment ago there was one. That is the design working, not
failing: shareability is a property of the *response*, and no route
configuration can override a response that addresses one person.

## Quick start

Harmost requires Rust 1.88 or newer.

```bash
git clone https://github.com/akosidencio/harmost.git
cd harmost

# Run the test suite.
cargo test --workspace

# Validate the example configuration.
cargo run -- check --config harmost.yaml

# Start the reverse proxy.
cargo run -- run --config harmost.yaml
```

The example listens on `0.0.0.0:8080` and forwards requests to the upstreams
defined in [`harmost.yaml`](./harmost.yaml). Update those upstream addresses
before sending production traffic.

To reproduce the bounded-concurrency demonstration, run:

```bash
./bench/demo.sh 60 1000
```

## Project status

Early-stage. Harmost runs as a Pingora reverse proxy with experimental cache
and request-coalescing integration.

| Area | State |
| --- | --- |
| Config schema, units, validation | done, tested |
| Route matching and policy snapshot | done, tested |
| Request classification (generic + Next.js) | done, tested |
| Cache key construction | done, tested |
| Response shareability rules | done, tested |
| Admission control, queueing, load shedding | done, tested |
| Cache store | implemented with Pingora `Storage` ([spike](./spike/pingora-cache/FINDINGS.md)) |
| Request coalescing | implemented with `pingora-cache` cache locks |
| Pingora proxy layer | proxy, routing, admission, upstream selection wired |
| Cache and coalescing wiring | implemented; experimental |
| Prometheus metrics, JSON access logs | done |
| Active health checks | done |
| Graceful reload (SIGHUP) | done |

The security- and correctness-sensitive parts—cache-key construction, response
shareability, and bounded admission—remain isolated as testable logic beneath
the Pingora proxy layer. The full workspace test suite runs without network
access.

## Design principles

**Uncertain means pass through.** If Harmost cannot prove a response is safe to
share, it does not share it. A higher hit ratio is never worth a wrong response.

**Reuse before admission.** Cache hits and coalescing waiters consume no origin
capacity, so they must never queue for it.

**Shareability is a property of the response, not the request.** A route that
looks public can still answer with a `Set-Cookie`. That check is absolute and no
configuration reaches past it.

**The cache is optional.** With `cache.enabled: false` you still get bounded
concurrency, bounded queues, load shedding, health-aware balancing and the
metrics. If the caching half were removed entirely this would still be worth
running.

## Request coalescing and microcache architecture

Harmost implements `pingora-cache`'s `Storage` trait rather than bringing its
own store, and uses its cache lock for coalescing. The
[spike](./spike/pingora-cache/FINDINGS.md) that settled this found that a waiter
can attach to the leader's write *in progress* and receive the first chunk
immediately, so collapsing duplicate requests does not cost every waiter the
full render latency.

It also found the sharp edge worth knowing: when a response turns out to be
uncacheable, the lock raises `GiveUp` and releases every waiting reader to the
origin at once. That is safe — an uncacheable response is never fanned out —
but it is an unbounded herd, and admission control is what bounds it. The
governor protects the cache, not the other way around.

## Next.js caching and cache-key design

### Explicit caching for dynamic Next.js routes

A dynamically rendered Next.js route answers `Cache-Control: private, no-cache, no-store,
max-age=0, must-revalidate`. Honouring that unconditionally — which is the
correct default — means the microcache never engages on precisely the pages
worth protecting. So a route may declare:

```yaml
- id: product-pages
  match: "/products/**"
  class: public_ssr          # required: you are asserting these are shareable
  cache:
    override_origin: true
    ttl:
      max: 2s                # required: an override with no ceiling is unbounded
```

It is per-route, never global, refuses to combine with a private class, and
still cannot override the absolute rules. Coalescing has a separate, weaker
override — collapsing duplicate renders persists nothing and lasts one render.

### Structural, collision-safe cache keys

The cache key is structural rather than hashed. For an in-process store, hashing
only buys a shorter map key and costs collision safety: two pages that collide
under a 64-bit hash are one user's document served to another. A short
fingerprint is derived separately, for logs.

## Configuration

Harmost uses YAML configuration for upstream servers, route matching, cache
policy, request coalescing, concurrency limits, queue deadlines, timeouts,
health checks, and telemetry. See the documented example in
[`harmost.yaml`](./harmost.yaml).

Unknown keys are a startup error rather than a silent no-op, and `harmost
check` refuses configurations that are syntactically valid but unsafe.

## License

Apache-2.0
