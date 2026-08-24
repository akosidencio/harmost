# Harmost

**Stop traffic spikes from becoming render spikes.**

A reverse proxy that bounds how much work a server-rendered origin is allowed to
do. It collapses duplicate concurrent renders, microcaches what is genuinely
shareable, and — the part that matters most — refuses to let more origin work be
in flight than the origin can survive.

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

## Why not just put a cache in front of it

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

## Status

Early. This is the correctness core, not yet a running proxy.

| Area | State |
| --- | --- |
| Config schema, units, validation | done, tested |
| Route matching and policy snapshot | done, tested |
| Request classification (generic + Next.js) | done, tested |
| Cache key construction | done, tested |
| Response shareability rules | done, tested |
| Admission control, queueing, load shedding | done, tested |
| Cache store | decided: implement Pingora's `Storage` ([spike](./spike/pingora-cache/FINDINGS.md)) |
| Request coalescing | decided: `pingora-cache`'s cache lock |
| Pingora proxy layer | proxy, routing, admission, upstream selection wired |
| Cache and coalescing wiring | not started |
| Prometheus metrics | not started |

```
cargo test --workspace                  # 85 tests, no network required
cargo run -- check --config harmost.yaml
cargo run -- run   --config harmost.yaml

./bench/demo.sh                         # the demonstration above
```

The proxy layer is deliberately absent from the dependency graph for now. The
three things worth getting right — key construction, shareability, and bounded
admission — are pure logic, and keeping them free of a proxy runtime keeps the
test loop instant and leaves the runtime decisions open.

## Design commitments

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

### Where the cache and the lock come from

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

### Two decisions that need explaining

**Next.js needs an explicit override to be cacheable at all.** A dynamically
rendered Next route answers `Cache-Control: private, no-cache, no-store,
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

**The cache key is structural, not hashed.** For an in-process store, hashing
only buys a shorter map key and costs collision safety: two pages that collide
under a 64-bit hash are one user's document served to another. A short
fingerprint is derived separately, for logs.

## Configuration

See [`harmost.yaml`](./harmost.yaml). Unknown keys are a startup error rather
than a silent no-op, and `harmost check` refuses configurations that are
syntactically valid but unsafe.

## License

Apache-2.0
