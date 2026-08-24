# Harmost: Rust Reverse Proxy for SSR Origin Protection

**Stop traffic spikes from becoming render spikes.**

> [!WARNING]
> **Harmost is under active development and is not ready for production.** It
> is in an early validation, verification, and testing phase. APIs,
> configuration, behavior, and operational guarantees may change without
> notice. Use it only in development or controlled test environments.

Harmost is an open-source Rust reverse proxy and origin workload governor for
server-rendered applications. It protects Next.js and other SSR origins with
bounded concurrency, request coalescing, safe microcaching, bounded queues, and
load shedding.

Unlike a conventional reverse-proxy cache, Harmost limits how much rendering
work may reach an origin at once. Cache hits and duplicate requests are reused
first; genuine cache misses enter per-route and global admission control before
they can consume origin capacity.

Harmost is built on [Pingora](https://github.com/cloudflare/pingora), the Rust
proxy framework Cloudflare wrote to replace its NGINX fleet. Cloudflare reports
that Pingora has served more than 40 million Internet requests per second for
years. The parts of a reverse proxy that are
unglamorous and easy to get subtly wrong — connection pooling, HTTP/1 and
HTTP/2 handling, graceful restarts, timeouts, the cache lock — are inherited
from that codebase. Harmost currently exercises only part of that surface; see
[Roadmap](#roadmap). What Harmost adds on top is the governor: classification,
cache-key and shareability rules, and bounded origin admission.

> A *harmost* (ἁρμοστής, from ἁρμόζω, "to fit, to keep in proper adjustment") was
> an official posted to hold a system in correct adjustment.

## Project maturity and expectations

**Harmost has not been run in production, by anyone, ever.** It has no
production users, no soak testing, no adversarial traffic history, and no
third-party security review.

Everything described below as a benefit is a **design goal supported by local
benchmarks**, not an outcome observed under real traffic. The numbers in this
README are reproducible on a laptop against a test origin that `sleep`s instead
of rendering — they demonstrate that the mechanisms work, and they say nothing
about how the system behaves against a real Next.js process, real network
conditions, or a real attacker.

Treat it as a working prototype with a serious test suite. If you deploy it, do
so behind something you can fail back to, and read
[Slow readers and render capacity](#slow-readers-and-render-capacity) and
[Roadmap](#roadmap) for the parts that are known to be incomplete.

## Contents

- [Project maturity and expectations](#project-maturity-and-expectations)
- [The problem](#the-problem)
- [Why Harmost exists](#why-harmost-exists)
- [Intended benefits](#intended-benefits)
- [What Harmost does](#what-harmost-does)
- [How Harmost works](#how-harmost-works)
- [Why use Harmost instead of a standard reverse-proxy cache?](#why-use-harmost-instead-of-a-standard-reverse-proxy-cache)
- [Admission control benchmark](#admission-control-benchmark)
- [Request coalescing benchmark](#request-coalescing-benchmark)
- [Streaming coalescing benchmark](#streaming-coalescing-benchmark)
- [Private-response safety check](#private-response-safety-check)
- [Quick start](#quick-start)
- [Installation](#installation)
- [Using Harmost with Next.js](#using-harmost-with-nextjs)
- [Project status](#project-status)
- [Roadmap](#roadmap)
- [Design principles](#design-principles)
- [Slow readers and render capacity](#slow-readers-and-render-capacity)
- [Request coalescing and microcache architecture](#request-coalescing-and-microcache-architecture)
- [Next.js caching and cache-key design](#nextjs-caching-and-cache-key-design)
- [Configuration](#configuration)
- [License](#license)

## The problem

A server-rendered request is not a cheap request. One hit on a Next.js route can
fan out into React rendering, a database round trip, a CMS call, a pricing
service and a recommendations service. Ten thousand requests can become ten
thousand of each.

That matters because SSR origins fail non-linearly. A Node process serving a
200 ms render comfortably at 50 concurrent requests does not serve 500 concurrent
requests ten times slower — it starts queueing inside the event loop, latency
climbs faster than load, health checks begin timing out, the orchestrator
restarts pods, and the surviving pods inherit the traffic that killed the last
ones.

Conventional protection is a poor fit for this shape:

- **Request-per-second rate limiting** counts requests, not work. A cheap static
  asset and an expensive product page cost the same against the limit.
- **A reverse-proxy cache** helps only where responses are cacheable. Next.js
  [documents dynamically rendered pages](https://nextjs.org/docs/app/guides/self-hosting#automatic-caching)
  as private and non-cacheable, so a conventional shared cache skips precisely
  the traffic that hurts.
- **Autoscaling** reacts on the order of tens of seconds. A traffic spike arrives
  in one.

## Why Harmost exists

Because the useful unit to bound is *origin work*, not requests, and few
off-the-shelf proxy configurations combine per-route work limits, a bounded
queue, a deadline, reuse-before-admission and a defined overload response.

Request collapsing is old news — Varnish, `proxy_cache_lock`, Fastly and Apache
Traffic Server have done it for years. Harmost's argument is the other half:
duplicate work is reused *first*, and whatever genuinely has to reach the origin
then passes through admission control that can say no.

## Intended benefits

Stated as goals, for the reasons in
[Project maturity](#project-maturity-and-expectations). Each links to the
benchmark that demonstrates the mechanism locally.

| Goal | Mechanism | Demonstrated by |
| --- | --- | --- |
| One Harmost process never admits more render work than its configured ceilings | Global and per-route concurrency limits with bounded queues | [`bench/demo.sh`](#admission-control-benchmark) |
| A burst on one URL costs one render, not thousands | Request coalescing on the cache lock | [`bench/coalesce.sh`](#request-coalescing-benchmark) |
| Collapsing does not destroy streaming | Waiters attach to the in-flight write | [`bench/stream.sh`](#streaming-coalescing-benchmark) |
| Overload degrades predictably instead of collapsing | Bounded queues, deadlines, stale-or-shed | [`bench/demo.sh`](#admission-control-benchmark) |
| Authorization-bearing requests and `Set-Cookie` responses are never shared | Absolute request/response barriers | [`bench/safety.sh`](#private-response-safety-check) |
| A slow client cannot make Harmost release render capacity early | Permits remain held until observed origin end-of-stream; downstream writes have a timeout | [`bench/slowclient.sh`](#slow-readers-and-render-capacity) |

What Harmost explicitly does **not** promise: lower latency. Bounding
concurrency makes a burst slower on purpose — see the wall-clock discussion in
[Why use Harmost](#why-use-harmost-instead-of-a-standard-reverse-proxy-cache).

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
  unsafe `Vary` headers, unsafe methods and unsafe statuses as absolute
  barriers. Origin `private`, `no-store` and `no-cache` directives are honoured
  unless a route is explicitly fenced as public and configured to override
  them. Cookie-bearing requests are private by default and likewise require an
  explicit route assertion before reuse is considered.
- **Balances requests across origins** with round-robin or path-stable hashing.

Typical use cases include protecting Next.js storefronts during product drops,
absorbing bursts on public SSR pages, and enforcing predictable origin capacity
for mixed public, private, static, and dynamic routes.

> [!NOTE]
> **Harmost is not a replacement for NGINX, Apache, Caddy or a general-purpose
> web server.** It is a specialised reverse proxy for governing SSR origin
> work. It does not currently provide the broad feature set expected from a
> general edge server, such as native TLS termination, full virtual-host/site
> configuration, static file serving, redirects and rewrites, WebSocket
> handling, authentication modules or a mature plugin ecosystem. Harmost may
> occupy the reverse-proxy hop in a narrow deployment, but it is not a drop-in
> substitute. A load balancer, ingress, CDN or conventional web server can
> remain in front of it for those responsibilities.

## How Harmost works

![Animated diagram showing how Harmost classifies requests, reuses cached or in-flight responses, bounds cache misses, and protects the SSR origin](./assets/harmost-flow.svg)

Harmost resolves the route and request class before checking for a safe cached
or in-flight response. Only a genuine cache miss enters the bounded per-route
and global admission queues. Accepted work reaches the SSR origin; excess work
receives an eligible stale response or is shed when the queue is full or its
deadline expires. Every origin response is checked again before it can be
shared or stored.

## Why use Harmost instead of a standard reverse-proxy cache?

Request collapsing is not new — Varnish, `proxy_cache_lock`, Fastly and Apache
Traffic Server have all done it for years, and forty lines of `nginx.conf` will
collapse a thousand concurrent hits on one URL down to a single origin request.

Harmost's intended distinction is the combination: reuse equivalent work
before applying per-route and global render ceilings, then use bounded queues
and stale-or-shed overload handling. Similar pieces exist elsewhere; Harmost
packages them around SSR work as one policy pipeline.

So the demo that matters is not the cacheable one.

## Admission control benchmark

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

## Streaming coalescing benchmark

Collapsing is only worth having if waiters still receive bytes as the leader
produces them. Otherwise every waiter but the leader trades a 1 ms first byte
for a full-render one.

```
$ ./bench/stream.sh 20 5 400

20 concurrent requests for one streaming url
(5 chunks, 400ms apart — the origin takes ~1600ms to finish)

  requests served       20
  median TTFB           0.002s
  max TTFB              0.009s
  median total          1.607s
  origin requests       1
```

One render, twenty responses, and every waiter held the shell within 9 ms while
that render was still in progress.

The proxy behavior was also checked from the fixture's `X-Origin-Total` header.
The current `stream.sh` log-based origin counter can incorrectly print `0`; the
reporter fix is the first roadmap phase and the sample above shows the verified
render count rather than that reporting error.

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

The mechanism claims in this README have local benchmark scripts. Each starts a
test origin and proxy, runs load, and tears both down:

```bash
./bench/demo.sh 60 1000     # bounded admission: origin peak 10 vs 60 direct
./bench/coalesce.sh 100     # 100 concurrent requests, one origin render
./bench/stream.sh 20 5 400  # collapsing without breaking streaming
./bench/safety.sh 50        # a Set-Cookie response is never shared
./bench/slowclient.sh       # diagnose slow-reader backpressure and permit lifetime
./bench/reload.sh           # SIGHUP reload, including a refused one
```

They measure against [`bench/slow-origin`](./bench/slow-origin), a fixture that
`sleep`s instead of rendering. That is enough to show the mechanisms work and
not enough to predict behaviour under real traffic — see
[Project maturity](#project-maturity-and-expectations).

## Installation

Building from source is the only supported route today. Harmost is not on
crates.io, there are no release binaries, and there is no container image.

```bash
git clone https://github.com/akosidencio/harmost.git
cd harmost
cargo build --release
```

The binary lands at `target/release/harmost`.

```bash
./target/release/harmost version
./target/release/harmost check --config harmost.yaml   # validate, don't start
./target/release/harmost run   --config harmost.yaml   # start the proxy
```

`harmost check` exits non-zero on an invalid or unsafe configuration, so it
works as a CI gate on a config change.

Signals: `SIGHUP` reloads reloadable policy in place and `SIGTERM` asks Pingora
for a graceful shutdown. Harmost does not yet expose Pingora's complete
zero-downtime upgrade workflow, so do not treat `SIGQUIT` alone as an upgrade
procedure.

The current listener and upstream connections are cleartext HTTP. Terminate TLS
at a load balancer or ingress in front of Harmost and keep the Harmost-to-origin
network private. Native downstream/upstream TLS and trusted-proxy handling are
roadmap items.

## Using Harmost with Next.js

> **This example is theoretical.** Harmost has not been run in front of a real
> Next.js application, and the configuration schema is pre-1.0 and expected to
> change. Nothing below has been validated against a production deployment —
> treat it as a worked illustration of the intended shape, not a deployment
> guide. See [Project maturity](#project-maturity-and-expectations).

### What you install, and where

Harmost is a **standalone binary that runs as its own process**, in front of
your Next.js server. It is not an npm package, not a dependency, not Next.js
middleware, and not something you import. **Your application code does not
change at all** — the only optional edit is adding a health endpoint, below.

What changes is the network path. Today:

```
client ──▶ next start (:3000)
```

With Harmost:

```
client ──▶ harmost (:8080) ──▶ next start (:3000)
```

Next.js stops being publicly reachable and listens only for Harmost. Whatever
used to point at Next — your load balancer, your CDN origin, your DNS record —
now points at Harmost instead.

#### One server

Both processes on the same box. Harmost takes the public port, Next binds to
loopback so nothing can reach it directly.

```yaml
server:
  listen: "0.0.0.0:8080"
origin:
  upstreams: ["127.0.0.1:3000"]
```

```bash
# terminal 1 — or a systemd unit / pm2 process
next start -p 3000 -H 127.0.0.1

# terminal 2
harmost run --config /etc/harmost/harmost.yaml
```

#### Docker Compose

Harmost is the only service publishing a port; `web` is reachable only on the
internal network. No image or Dockerfile ships with the project yet, so the
image here is one you build from source yourself.

```yaml
services:
  harmost:
    image: harmost:local          # built from source; nothing is published
    ports: ["8080:8080"]
    volumes:
      - ./harmost.yaml:/etc/harmost/harmost.yaml:ro
    command: ["run", "--config", "/etc/harmost/harmost.yaml"]
    depends_on: [web]

  web:
    image: my-next-app
    expose: ["3000"]        # not `ports` — no host publishing
```

with `upstreams: ["web:3000"]`.

#### Kubernetes

Harmost is its own Deployment and Service, between the Ingress and the Next
Service. The Next Service becomes `ClusterIP` and is no longer an Ingress
backend.

```
Ingress ──▶ Service/harmost ──▶ Deployment/harmost (2 replicas)
                                        │
                                        ▼
                               Service/web ──▶ Deployment/web (N pods)
```

with `upstreams: ["web.default.svc.cluster.local:3000"]`.

Keep the Harmost replica count low. Coalescing only collapses requests that
reach the *same* instance, so replicas divide the benefit — and an autoscaler
that adds replicas during a spike reduces collapsing exactly when it is most
wanted. Concurrency limits are also per process: two replicas configured with a
ceiling of 100 can admit up to 200 origin requests between them. Size the
per-process limit accordingly. If you need more than a few replicas, have the
Ingress consistent-hash on path so one key lands on one instance.

#### Where this does not work

**A normal Vercel, Netlify, or similar managed deployment.** Those platforms do
not provide a place to run Harmost immediately beside the application origin.
Harmost currently targets infrastructure where you control both hops — a VPS,
ECS, Fly, Kubernetes or bare metal — with optional CDN/TLS termination in
front.

### A worked configuration

```yaml
version: 1

server:
  listen: "0.0.0.0:8080"

origin:
  upstreams:
    - "next-1:3000"
    - "next-2:3000"
  # Sends a given path to the same instance, which also warms Next's own
  # in-process render cache and JIT state.
  load_balancing: hash_by_path
  concurrency:
    max: 200                 # per Harmost process, across this upstream pool
    queue:
      max: 1000
      timeout: 2s

cache:
  enabled: true
  max_memory: 512MiB
  max_body_size: 4MiB

routes:
  # Fingerprinted assets. Immutable, freely shareable, and exempt from the
  # render budget — serving a JS chunk is not rendering.
  - id: next-static
    match: "/_next/static/**"
    class: static

  # The image optimiser is expensive and its output is keyed by query.
  - id: next-image
    match: "/_next/image"
    class: public_dynamic
    cache:
      ttl:
        max: 60s
      query:
        mode: include
        keys: ["url", "w", "q"]
    concurrency:
      max: 20

  # Public product pages. A dynamically rendered Next route answers
  # `Cache-Control: private, no-store`, so `override_origin` is required or
  # nothing here is ever cached or collapsed.
  - id: products
    match: "/products/**"
    class: public_ssr
    cache:
      override_origin: true
      ttl:
        max: 2s
      stale_while_revalidate: 30s
      stale_if_error: 5m
    coalesce:
      enabled: true
    concurrency:
      max: 100
      queue:
        max: 300
        timeout: 1500ms

  # Per-user routes. Never cached, never collapsed — but still bounded, which
  # is most of the protection.
  - id: account
    match: "/account/**"
    class: private_dynamic
    cache:
      enabled: false
    concurrency:
      max: 40
      queue:
        max: 100
        timeout: 1s

  # Anything not named above is treated as private. Make the unsafe case the
  # one you have to opt into, not the one you get by forgetting.
  - id: default
    match: "/**"
    class: private_dynamic
    cache:
      enabled: false

telemetry:
  prometheus:
    listen: "127.0.0.1:9090"
  logging:
    format: json
```

### What the Next.js adapter handles for you

The adapter applies these classifications automatically. Route and response
policy still decide whether reuse is ultimately allowed:

- **React Server Components.** The same URL returns HTML or an RSC flight
  payload depending on the `RSC` header, so `RSC`, `Next-Router-Prefetch`,
  `Next-Router-State-Tree` and `Next-Url` are part of the cache key. Without
  that, a flight payload eventually gets served to a browser as a document.
- **Prefetch requests** are marked coalesce-only and never stored. They are
  collapsed only when the route and response policy permit sharing; the router
  state tree makes persistent storage too high-cardinality.
- **Server Actions** (`POST` carrying `Next-Action`) are classified as
  mutations and bypass cache reuse and coalescing. They remain admission
  controlled.
- **Draft mode.** A request carrying `__prerender_bypass` or
  `__next_preview_data` bypasses reuse and is treated as private, so unpublished
  content is never stored or shared. It remains admission controlled.

### Two things that will bite you

**Next.js has no guaranteed built-in health endpoint.** The repository's
[`harmost.yaml`](./harmost.yaml) probes `/healthz`, which a stock application
does not automatically provide — every backend would fail its probe. Add one:

```ts
// app/healthz/route.ts
export const dynamic = "force-dynamic";
export function GET() {
  return new Response("ok");
}
```

Or omit the `health:` block entirely. Harmost still serves when nothing is
healthy, on the grounds that refusing to pick turns a degraded origin into a
guaranteed outage — so a misconfigured probe degrades quietly rather than
loudly, which is exactly how it goes unnoticed.

**Do not put Harmost in front of `next dev`.** `Upgrade`/WebSocket handling is
not implemented, so hot module reload will not work. Use it in front of
`next start`.

### Verifying it is doing anything

```bash
curl -sI localhost:8080/products/example | grep -i x-harmost   # needs debug_headers: true
curl -s localhost:9090/metrics | grep -E 'reuse_eligible|origin_requests'
```

The second pair is the ratio worth watching: `harmost_origin_requests_total`
over `harmost_reuse_eligible_requests_total` is the share of eligible traffic
that still reached the origin.

## Project status

**Not production ready.** Harmost is under active development and remains in an
early validation, verification, and testing phase. It runs as a Pingora reverse
proxy with experimental cache and request-coalescing integration, but it has
not completed production hardening or operational validation.

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
| Prometheus metrics, JSON/text access logs | done |
| Active health checks | done |
| Graceful reload (SIGHUP) | done |
| Stale-while-revalidate, stale-if-error | done |
| Circuit breaking, least-loaded balancing | not started ([roadmap](#roadmap)) |
| Cache purge API, OpenTelemetry | not started ([roadmap](#roadmap)) |

The security- and correctness-sensitive parts — cache-key construction, response
shareability, and bounded admission — remain isolated as testable logic beneath
the Pingora proxy layer. The full workspace test suite (133 tests) runs without
external network services.

"done, tested" means unit-tested and, where a `bench/` script exists, verified
end to end against a local test origin. It does not mean production-validated;
see [Project maturity](#project-maturity-and-expectations).

Configuration options that are accepted but not yet implemented are rejected at
startup rather than silently ignored, so a config can never claim a protection
that is not running.

## Roadmap

Ordered by dependency and risk rather than novelty. Later phases depend on the
evidence and safety work before them.

### 0. Make the evidence trustworthy

- Replace broad process-killing benchmark cleanup with exact PID tracking,
  dynamic ports and machine-checked assertions. Fix the streaming origin-count
  and reload-success reporters.
- Add a real Next.js fixture covering the App Router, Pages Router, RSC,
  prefetch, Server Actions, Draft Mode, streaming and `Set-Cookie` responses.
- Run unit and integration tests in CI on Linux, with a smaller macOS build
  matrix; publish benchmark parameters with results rather than treating one
  laptop run as a baseline.
- Add property tests and fuzz targets for cache-key canonicalisation,
  `Cache-Control`, `Vary`, cookies and malformed HTTP metadata.

### 1. Close protocol and security gaps

- Exercise downstream and upstream HTTP/2 end to end, then add explicit tests
  for `HEAD`, `Range`, conditional requests, disconnects and malformed bodies.
- Support `Upgrade`/WebSocket traffic without weakening admission or cache
  rules.
- Add native downstream and upstream TLS, configurable trusted proxies, and
  correct forwarded scheme/client-IP handling.
- Write a threat model, run sustained adversarial tests, and obtain an
  independent review of cache keys and response shareability.
- Add a bounded response spool so a slow reader cannot retain a render permit
  after the origin has actually finished.

### 2. Become operable as a service

- Ship versioned release binaries and an OCI image with checksums, an SBOM and
  reproducible build instructions.
- Add readiness and administrative status endpoints that expose configuration
  generation, backend state, cache usage and drain state without exposing
  client-controlled cardinality.
- Add OpenTelemetry spans and request correlation alongside the existing
  Prometheus metrics and structured access logs.
- Expose and test Pingora's complete zero-downtime upgrade workflow; document
  systemd and Kubernetes drain procedures.
- Version the configuration schema and provide migration notes before the
  pre-1.0 format becomes an operational dependency.
- Add soak, memory-pressure, restart and chaos tests with explicit release
  gates, plus example Prometheus alerts and dashboards.

### 3. Improve origin resilience

- Add passive failure observation, per-backend circuit breakers and outlier
  ejection alongside active health checks.
- Add retry budgets for eligible idempotent requests only; never retry
  mutations blindly.
- Add least-loaded selection using in-flight work and latency observations.
- Add weighted admission, route priorities and reserved capacity so a slow,
  expensive route cannot starve cheap critical work.

### 4. Complete cache lifecycle and framework integration

- Add a purge API and cache tags, including deployment-safe invalidation and a
  path from Next.js `revalidateTag()`/`revalidatePath()` events.
- Replace FIFO eviction with a measured production policy and evaluate optional
  disk or external storage without making it required for admission control.
- Build a versioned `@harmost/next` integration that exports route hints,
  deployment ids and invalidation events; maintain a tested Next.js
  compatibility matrix.
- Add adapters only after the generic policy contract is stable, starting with
  frameworks that expose reliable route and privacy metadata.

### 5. Adapt and scale deliberately

- Evaluate adaptive concurrency only after latency and failure signals are
  trustworthy; retain hard operator-defined ceilings as safety rails.
- Define a multi-instance capacity model so replicas cannot accidentally
  multiply the intended origin ceiling.
- Keep distributed coalescing optional. Path-stable ingress routing remains the
  simpler default; a distributed lock is justified only by measured need.

### Current limitations and non-goals

- A slow reader can delay observed origin end-of-stream and occupy a render
  slot. `timeouts.downstream_write` bounds the stall but does not decouple it;
  see [Slow readers and render capacity](#slow-readers-and-render-capacity).
- Cache, coalescing and admission state are process-local. Replicas multiply
  origin ceilings unless their limits are partitioned, and one key can render
  once per replica.
- The only cache backend is bounded in-process memory. Restarting clears it,
  and FIFO eviction is intentionally simple rather than production-tuned.
- Harmost does not terminate TLS, connect to TLS origins, or configure trusted
  proxy boundaries yet.
- `serde_yaml`, which is deprecated, still enters the dependency tree through
  `pingora-core`'s own config parsing. Harmost's config is parsed with
  `serde-saphyr`.

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

**A permit models observable origin work.** Origin capacity is returned only
when Pingora observes upstream end-of-stream. A `Content-Length` is not proof
that generation has finished, so headers alone never release capacity.

## Slow readers and render capacity

Pingora couples upstream reads to bounded downstream channels. A sufficiently
slow client can therefore delay the moment upstream end-of-stream is observed,
for both fixed-length and chunked responses. `timeouts.downstream_write` bounds
that delay. Decoupling it completely requires a separate bounded response spool;
until then the conservative choice is to hold the permit rather than let a
fixed-length streaming origin bypass the concurrency ceiling.

Each access log line records `"permit_released":"body_end"` when capacity was
returned normally.

## Request coalescing and microcache architecture

Harmost provides a bounded in-memory implementation of `pingora-cache`'s
`Storage` trait and uses Pingora's cache lock for coalescing. The
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

### Structural cache keys

Harmost builds the cache key as a *structure* — scheme, host, method, path,
canonicalised query, the headers that select a variant, and the deployment id —
rather than folding those into a number early. Query parameters are sorted only
when keys are unique; duplicate-key order and `?flag` versus `?flag=` remain
distinct. `Accept-Encoding` retains coding order and quality values, configured
`Vary` headers join the framework variants, and the Next.js RSC headers are
included so a flight payload can never be served as an HTML document.

That structure is then rendered to one canonical string, with a separator that
cannot occur inside any component so `host=a.com path=/b` cannot collide with
`host=a.com/b path=`. Pingora hashes that string (128-bit) at the storage
boundary, which is where the key finally becomes opaque.

## Configuration

Harmost uses YAML configuration for upstream servers, route matching, cache
policy, request coalescing, concurrency limits, queue deadlines, timeouts,
health checks, and telemetry. See the documented example in
[`harmost.yaml`](./harmost.yaml).

`harmost check --config <file>` validates without starting the proxy, which
makes it usable as a CI gate.

Three rules govern the config surface:

- **Unknown keys are a startup error**, not a silent no-op. A typo'd key would
  otherwise be an invisible policy change.
- **Options that are not yet implemented are rejected**, so a config can never
  claim a protection that is not running.
- **Syntactically valid but unsafe configurations are refused.** Marking a
  route `private_dynamic` and then enabling a cache override on it, or setting
  a coalescing wait shorter than the origin timeout, both fail at startup with
  an explanation rather than at 3am with an incident.

Configuration is parsed with [`serde-saphyr`](https://crates.io/crates/serde-saphyr):
pure Rust, actively maintained, and it reports the line and column of a bad key.

## License

Harmost is licensed under the [Apache License 2.0](./LICENSE).
