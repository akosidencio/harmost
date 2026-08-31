# Changelog

All notable changes to Harmost. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

**Versioning.** Harmost is pre-1.0 and follows Cargo's pre-1.0 SemVer
convention: `0.x.y` may change behaviour in a `y` bump, and every such change
is listed here. It is a binary rather than a library, so the surface that
matters is the **configuration file** and the **operator interface** — the
config schema version, the CLI, the signals, the metric names and the admin
endpoints. Changes to those are called out explicitly.

**Configuration schema.** Separate from the release version and currently
**version 1**, unchanged since the first release. The rules for what may change
without a schema bump are in [`docs/CONFIG-SCHEMA.md`](./docs/CONFIG-SCHEMA.md).

**Maturity.** No release has been run in production by anyone. "Done, tested"
throughout this file means unit-tested and, where a `bench/` script exists,
asserted end to end against a local fixture origin. See
[Project maturity](./README.md#project-maturity-and-expectations).

---

## [0.1.2] — unreleased

Roadmap phase 3: origin resilience. Everything here is off or inert by default,
so no existing config changes behaviour.

### Added

- **Per-backend circuit breakers** (`origin.breaker`), fed by passive
  observation of real traffic rather than a probe. Catches the origin that
  answers `/healthz` while every render fails.
- **Outlier ejection cap** (`origin.breaker.max_ejected_percent`). Past it,
  breaker state is ignored and routing falls back to health — an origin-wide
  failure must not leave Harmost with nowhere to send anything.
- **Recovery probes.** One request per `open_for` is spent testing an ejected
  backend, so a blip cannot eject it permanently.
- **Bounded retries** (`origin.retry`). Safe methods only, only before the
  origin has answered, and capped by a budget measured as a share of live
  traffic. A retry re-enters peer selection, so it lands on a different
  backend.
- **`origin.load_balancing: least_loaded`**, scoring in-flight work against
  EWMA time-to-first-byte. Routes around a backend that is up but slow.
- **Route priorities and reserved capacity** (`origin.priorities`,
  `route.priority`). Each tier gets a ceiling as a percentage of
  `origin.concurrency.max`; reserved capacity is the complement.
- **Weighted admission** (`route.weight`). A ceiling counts units of origin
  work instead of requests.
- **Metrics:** `harmost_upstream_ejected`,
  `harmost_upstream_breaker_trips_total`, `harmost_upstream_failures_total`,
  `harmost_upstream_in_flight`, `harmost_upstream_latency_ewma_microseconds`,
  `harmost_origin_retries_total`, `harmost_origin_retry_budget`. Priority tiers
  publish under the existing `limiter` label as `tier:high` / `tier:normal` /
  `tier:low`.
- **`/status`** reports per-backend ejection, in-flight, EWMA latency, breaker
  window counts and trips; plus tier ceilings and retry-budget state.
- **Benchmarks:** [`bench/breaker.sh`](./bench/breaker.sh) and
  [`bench/retry.sh`](./bench/retry.sh), both asserting on what the origin
  fixtures counted. `bench/slow-origin` gained `/__fail` and `/__heal`.
- **Alerts** for the above in [`ops/prometheus/alerts.yml`](./ops/prometheus/alerts.yml).

### Changed

- `harmost check` prints the breaker, retry and priority settings in force.
- `SIGHUP` refuses `origin.breaker` and `origin.retry`: both carry rolling
  windows bound at startup. `origin.priorities`, `route.priority` and
  `route.weight` **do** reload.
- Configurations that would silently do nothing are refused at startup: a
  breaker with one upstream, an ejection cap that floors to zero backends,
  `max_attempts: 1`, a zero retry budget, a priority share that floors to a
  ceiling of zero, `route.priority` with uniform `origin.priorities`, and a
  `route.weight` above any ceiling it is charged against.

### Known limitations

- Breaker and retry-budget state is process-local, like the cache and the
  admission ceilings. Each replica must observe a failing backend for itself,
  and *n* replicas have *n* retry budgets.
- Priority tiers isolate ceilings, not queues. Waiting is FIFO within a tier.
- Any 5xx counts as an origin failure. An application that 500s by design will
  move breakers no backend deserves.
- Passive observation records one outcome per attempt, decided by the response
  header. A body error after a successful header counts as the success it began
  as.

### Upgrading from 0.1.1

No configuration change is required. If you run more than one backend,
`origin.breaker` is the piece worth adding first — watch
`harmost_upstream_ejected` before relying on it. Leave `origin.retry` off until
`harmost_upstream_failures_total` says what is actually failing.

---

## [0.1.1] — 2026-08-29

Two roadmap phases: closing the protocol and security gaps that made the first
release unsafe to put in front of real traffic, and making the result operable
as a service.

### Added

**Protocol coverage**

- **HTTP/2, both ends.** `server.h2c` accepts cleartext HTTP/2 on the ordinary
  listener (Pingora peeks for the connection preface, so HTTP/1.1 clients are
  unaffected); `server.tls.h2` offers `h2` over ALPN; `origin.http_version`
  chooses what Harmost speaks upstream. ([`bench/http2.sh`](./bench/http2.sh))
- **Native TLS**, behind `--features tls`: `server.tls` terminates, `origin.tls`
  connects. rustls rather than boringssl or openssl, so the build needs no
  cmake, no Go and no system OpenSSL headers. A binary built *without* the
  feature **rejects** a config containing either block rather than quietly
  serving cleartext. ([`bench/tls.sh`](./bench/tls.sh))
- **`Upgrade`/WebSocket proxying.** Off by default and answered `501` when off.
  When on, an upgrade takes `upgrade.max_concurrent` rather than a render
  permit — a tunnel is held for minutes and a render for milliseconds, so
  sharing a budget would let a handful of sockets starve every page. Never
  cached, never coalesced. ([`bench/websocket.sh`](./bench/websocket.sh))
- **`server.trusted_proxies`**, gating every forwarded header behind the
  connection peer's address. Trusts nobody by default, and walks the hop chain
  from the right. ([`bench/forwarded.sh`](./bench/forwarded.sh))
- **A bounded response spool** (`spool.*`, off by default, per route). Origin
  capacity now returns when the origin finishes rather than when the client
  finishes reading. Costs progressive rendering, which is why it is opt-in.
  ([`bench/spool.sh`](./bench/spool.sh))

**Operability**

- **Readiness, liveness and status endpoints** on a listener of their own
  (`telemetry.admin`): `/health/live`, `/health/ready`, `/status`. `/status`
  reports configuration generation and a stable effective-config fingerprint,
  per-backend health, cache and spool occupancy, admission limits and in-flight
  counts, drain state and compiled features. Nothing on the surface is
  parameterised — no path, query or header
  changes what is computed, the same rule the metric labels follow. Startup
  refuses to share the address with the traffic or metrics listener.
  ([`bench/admin.sh`](./bench/admin.sh))
- **Drain state.** `SIGUSR1` drains **without exiting**: readiness begins
  answering `503` immediately while the process keeps serving normally, so a
  load balancer can withdraw the instance before anything stops. This is the
  signal a Kubernetes `preStop` hook should send.
- **Zero-downtime restart.** `harmost run --upgrade` performs Pingora's
  listening-socket handover, `--test` is the pre-flight that proves a new
  binary can start before the old one is signalled, and `--daemon` forks with a
  pid file. Configured under `server.graceful`.
  ([`bench/upgrade.sh`](./bench/upgrade.sh))
- **W3C trace-context correlation, unconditional and free.** Every request gets
  a trace id and span id; both appear on every access log line alongside the
  configuration generation that served it; and the id Harmost concluded is what
  reaches the origin as `traceparent`. Inbound trace context is ignored by
  default. `from_trusted_proxies` is opt-in because trace context has no hop
  chain: every trusted proxy must strip or replace client-supplied trace
  headers. Ignoring one never costs the request.
- **OpenTelemetry span export** (`telemetry.tracing.otlp`), sampled. Two spans
  per request: a server span, and a nested origin-fetch span so an origin
  latency number has something to hang off. ([`bench/tracing.sh`](./bench/tracing.sh))
- **Configuration schema versioning.** `version:` is checked against a constant
  and an unknown version is refused with both numbers in the message.
  [`docs/CONFIG-SCHEMA.md`](./docs/CONFIG-SCHEMA.md) states what may change
  without a bump and what may not.
- **Soak, memory-pressure, restart and chaos tests** —
  [`soak.sh`](./bench/soak.sh), [`memory.sh`](./bench/memory.sh),
  [`upgrade.sh`](./bench/upgrade.sh), [`chaos.sh`](./bench/chaos.sh) — CI-sized
  on every push and full-size before a tag, with the gates written down in
  [`docs/RELEASE-GATES.md`](./docs/RELEASE-GATES.md).
- **New metrics:** `harmost_spans_total{outcome}`, `harmost_draining`,
  `harmost_config_generation`, `harmost_config_fingerprint`,
  `harmost_upstream_healthy{upstream}`,
  `harmost_cache_max_bytes`, `harmost_spool_max_bytes`. The last two exist so a
  dashboard has a denominator for the occupancy gauges rather than a hardcoded
  ceiling that goes stale the first time somebody edits the budget.
- **Release artifacts:** a versioned `x86_64-unknown-linux-gnu` binary, a
  `SHA256SUMS` covering every archive, a CycloneDX SBOM, and an OCI image with
  provenance and its own SBOM attached as attestations.
- **Documentation:** [`docs/OPERATIONS.md`](./docs/OPERATIONS.md),
  [`docs/CONFIG-SCHEMA.md`](./docs/CONFIG-SCHEMA.md),
  [`docs/RELEASE-GATES.md`](./docs/RELEASE-GATES.md),
  [`docs/THREAT-MODEL.md`](./docs/THREAT-MODEL.md),
  [`docs/CACHE-KEY-REVIEW.md`](./docs/CACHE-KEY-REVIEW.md), and example
  Prometheus alerts and a Grafana dashboard in [`ops/`](./ops).

### Changed

- **Access log lines carry four new fields**: `trace_id`, `span_id`,
  `trace_continued` and `generation`. Additive, but anything parsing the line
  positionally rather than as JSON will need updating.
- **`origin.tls.ca` is accepted by the schema and rejected at startup.**
  Pingora 0.8's rustls connector never reads a per-peer CA store — its
  `connect` path carries an explicit `TODO` and `peer.get_ca()` is unused — so
  honouring the key is impossible and ignoring it would mean a config naming a
  CA, a proxy verifying against the system roots, and no way to tell from
  outside. Use `SSL_CERT_FILE` / `SSL_CERT_DIR`.
- **`SIGHUP` refuses more settings rather than silently ignoring them.** Added
  to the startup-bound list: `server.trusted_proxies`, `server.h2c`,
  `server.tls`, `origin.tls`, `origin.http_version`, `spool.max_memory`,
  `upgrade.max_concurrent`, `server.graceful`, `telemetry.admin`, and
  `telemetry.tracing.otlp` / `service_name`. A reload reporting success while
  the old trust policy stayed in force is the worst version of this.
  `telemetry.tracing.sample` and `trust_incoming` **do** reload, because
  turning sampling down is an incident action.
- **Route limiters are created at startup** rather than on a route's first
  request, so `/status` reports the configured policy from the first scrape.
- **`harmost version` prints the config schema version and compiled features**,
  because a binary that rejects `server.tls` and one that terminates it are
  otherwise the same version number.
- The crate now forbids `unsafe_code` through `[lints.rust]`, denies `unwrap`, `expect` and
  lossy casts, pins the toolchain in `rust-toolchain.toml`, and runs
  `cargo audit` as a CI gate with every ignored advisory carrying a reason and
  a way out in [`.cargo/audit.toml`](./.cargo/audit.toml).

### Fixed

- **Over HTTP/2 the cache key had no host, merging every virtual host on the
  listener into one entry.** There is no `Host` header in HTTP/2; the authority
  is the `:authority` pseudo-header, which Pingora surfaces on the URI. Reading
  `Host` alone gave every h2 request an empty host. A cross-tenant response
  leak that appears the day `server.h2c` or `server.tls` is switched on and is
  invisible before then. *(Not reachable in 0.1.0, which had no HTTP/2.)*
- **`X-Forwarded-For` was appended to rather than replaced**, so whatever the
  client sent stayed in first position — where every framework's `getClientIp`
  reads. Since the origin's rate limits and audit logs are downstream of that,
  it was a forged identity with real effects. Now `insert_header`, and RFC 7239
  `Forwarded` is removed outright.
- **`X-Forwarded-Proto` was hardcoded to `http`,** and the cache key's scheme
  with it. Correct while Harmost only ever spoke cleartext; wrong the moment it
  terminates TLS, and the symptom is a plaintext response served to a client
  that asked for TLS.
- **A shrinking concurrency limit could be escaped by queued requests.**
  `Semaphore::forget_permits` can only take *available* permits, so a shrink
  while requests are in flight carries a debt — and the window between
  recording it and settling it let queued requests steal every returned permit
  and hold concurrency near the old ceiling for another generation of work.
- **Lossy duration and byte-size casts** could turn a configured budget into a
  different one on a target with narrower pointers. Every remaining `as` is now
  clamped at the call site or carries a scoped allow naming why it cannot lose
  data.
- **A queue deadline longer than a representable `Instant`** is refused at
  startup instead of overflowing.
- **`concurrency.max` above tokio's semaphore maximum** is refused at startup
  instead of panicking on first use.
- **`server.tls` cert and key files that exist but cannot be loaded** are
  refused at startup instead of failing on the first TLS handshake.
- **`cargo fuzz` invocations in CI** are pinned to `+nightly`, so
  `rust-toolchain.toml` cannot redirect them to a stable compiler that cannot
  build them.
- **Plain `SIGTERM` now drains before Pingora receives shutdown.** Readiness
  returns `503` while traffic listeners remain available for the configured
  drain window. A prior `SIGUSR1` is credited, so a pre-stop hook does not pay
  the same window twice.
- **Sub-second shutdown timeouts round upward** when converted to Pingora's
  whole-second setting instead of silently becoming zero.
- **Strict upstream readiness begins unknown, not healthy.** Enabling
  `require_healthy_upstream` now requires an active health checker and remains
  not-ready until a backend completes its configured successful-probe streak.
- **The OTLP shutdown flush has one total deadline**, rather than multiplying
  the configured timeout by the number of queued batches. Endpoint parsing
  also rejects whitespace and control characters before building an HTTP
  request line.
- **Admin listener collision checks include wildcard binds**, including the
  platform-dependent dual-stack behaviour of `[::]`.
- **Fleet config drift compares a stable configuration fingerprint.** Reload
  generation remains a per-process event counter and is no longer incorrectly
  grouped as though its value were a Prometheus label.
- **The default image creates `/run/harmost`**, and pid-file documentation now
  states that foreground processes do not write one.
- **Release publishing is pinned to an immutable action commit**, and the
  reproducible-build helper derives checkout and Cargo paths instead of
  embedding one CI runner's filesystem layout.

### Known limitations introduced or clarified

- **The zero-downtime socket handover only works on Linux.** Pingora passes
  listening descriptors with `SCM_RIGHTS`; its non-Linux `get_fds_from` is a
  stub that returns `ECONNREFUSED`, which reads exactly like "no old process is
  listening". Harmost refuses `--upgrade` elsewhere and names the real reason.
  The drain-based restart in [`docs/OPERATIONS.md`](./docs/OPERATIONS.md) works
  everywhere but relies on a load balancer to cover the gap between the old
  process exiting and the new one binding.
- **A direct `SIGTERM` costs `drain_period + shutdown_timeout` even on an idle
  process.** Harmost keeps its listeners available for the first window, then
  Pingora keeps the runtime shutdown window open whether or not anything is in
  flight. If `SIGUSR1` already began draining, only its unelapsed remainder is
  waited. The defaults total 15 seconds for a direct stop — inside Kubernetes'
  default `terminationGracePeriodSeconds: 30` — and `harmost check` prints the
  sum and warns above 30.
- **The OTLP exporter is plaintext-only** and an `https://` endpoint is
  refused at startup rather than quietly downgraded. It is hand-written
  (OTLP/HTTP, JSON encoding) because `opentelemetry-otlp` brings either a gRPC
  stack or an HTTP-client stack larger than the rest of this binary. Run an
  OpenTelemetry Collector as a sidecar.
- **No release has actually been cut.** The workflow, checksums, SBOM and
  reproducible-build instructions are written and their steps individually
  exercised, but nobody — including the author — has downloaded a published
  artifact and reproduced it. Until then, treat the reproducible-build
  workflow as a specification rather than a report.
- **The independent review of cache-key construction and response
  shareability is still not obtained.** It is not something the author can
  complete alone; [`docs/CACHE-KEY-REVIEW.md`](./docs/CACHE-KEY-REVIEW.md) is
  written to make one cheap.

### Upgrading from 0.1.0

No configuration change is required: every new key is optional and defaults to
the previous behaviour. Four things are worth doing anyway.

1. **Add `telemetry.admin`.** Without it there is no readiness endpoint, so
   nothing can tell when an instance is draining and a rolling restart drops
   requests. `harmost check` now says so.
2. **Set `server.graceful.pid_file` and `upgrade_socket` per instance.** They
   default to `/tmp`, and two Harmost processes on one host with the defaults
   will hand each other their listening sockets. The pid file is created only
   with `--daemon`; foreground supervisors should signal their tracked PID.
3. **Check your supervisor's stop timeout** against
   `drain_period + shutdown_timeout` — see the limitation above.
4. **Run `harmost check`** before deploying. Three configurations that
   previously started and then failed at runtime are now refused at startup: a
   queue deadline that is not a representable instant, a `concurrency.max`
   above tokio's semaphore maximum, and `server.tls` files that exist but
   cannot be loaded. A config that hits any of these was never working.

---

## [0.1.0] — 2026-08-27

First tagged release. The three primitives, the Pingora proxy layer on top of
them, and the evidence work that makes the benchmark suite worth reading.

### Added

- **Admission control**: a per-route and global ceiling on in-flight origin
  work, a bounded queue with a deadline, and load shedding on overflow. The
  defensible pillar — request collapsing is commoditised; bounded origin-work
  admission is not.
- **A microcache** built on `pingora-cache`'s `Storage` trait, with a bounded
  in-process store, FIFO eviction and a byte budget.
- **Request coalescing**, including on streaming responses: concurrent
  equivalent requests collapse to one origin render, and a waiter attaches to
  the in-flight write rather than waiting for it to finish.
- **`cache.override_origin`**, fenced per route. A dynamically rendered Next.js
  route answers `Cache-Control: private, no-cache, no-store, max-age=0,
  must-revalidate`, so without an override the microcache never engages on
  precisely the pages worth protecting. Validation refuses to combine it with a
  private class, and no override reaches past a `Set-Cookie` response.
- **Stale-while-revalidate and stale-if-error.**
- **Active health checking** with streak-based state changes, so one bad probe
  during a GC pause does not drain a healthy backend.
- **Round-robin and hash-by-path** upstream selection. Hashing by path also
  warms the origin's own render cache and JIT state.
- **`SIGHUP` configuration reload.** An invalid config is refused and the
  running one keeps serving; limiters resize in place rather than being
  swapped, which would transiently double admitted concurrency.
- **Prometheus metrics and structured JSON/text access logs**, with the query
  string deliberately absent from both.
- **A real Next.js fixture** serving both the App Router and the Pages Router,
  a containerised integration proof, and browser-driven checks for the two
  requests curl cannot construct — a real router prefetch and a real Server
  Action submission.
- **An asserting benchmark harness** ([`bench/lib.sh`](./bench/lib.sh)): exact
  pid tracking, ports allocated per run, polled readiness, and a JSON report
  carrying the parameters that produced each result. Every script exits
  non-zero on failure, so the suite is a gate rather than a report.
- **Property tests and six fuzz targets** over cache-key canonicalisation,
  `Cache-Control`, `Vary`, cookies and malformed HTTP metadata.

### Fixed

- **`HeaderValue::to_str` refuses obs-text that `HeaderValue` itself accepts**,
  and every caller dropped the value on failure. One non-ASCII byte in an
  unrelated cookie hid every cookie in the header — including
  `__prerender_bypass`, so a Next.js draft-mode render was cached and served
  publicly while Next itself, parsing the same bytes, honoured the cookie. The
  same pattern made an unreadable `Next-Action` classify as a cacheable
  document, an unreadable prefetch header make a near-unbounded key space
  storable, and a non-ASCII variant value collapse to "header absent" so
  clients asking for different things shared one entry.
- **The cache key rendered `deployment: None` and `deployment: Some("")`
  identically.** Two structurally distinct keys shared one entry. The canonical
  encoding is now length-prefixed rather than merely separator-delimited, so
  its injectivity is a property of the function rather than of `http`'s input
  validation.
- **`should_serve_stale` must be implemented or stale-while-revalidate
  silently does nothing.** Pingora's default answers on an upstream error, and
  SWR calls it with no error at all.
- **Coalescing failed silently on streaming responses.** When
  `support_streaming_partial_write()` is true, Pingora releases the cache lock
  as soon as the leader's miss handler exists, so waiters retry a plain
  `lookup()` with no write tag — which only checked finished entries, sending
  every waiter to the origin.

### Known limitations at this release

- A slow reader delayed observed origin end-of-stream and occupied a render
  slot, bounded only by `timeouts.downstream_write`. *(Fixed in 0.1.1 by the
  response spool.)*
- No HTTP/2, no TLS, no `Upgrade` support, and no way to distinguish a trusted
  proxy from a client. *(All added in 0.1.1.)*
- Cache, coalescing and admission state are process-local. Replicas multiply
  origin ceilings unless their limits are partitioned.
- No rate limiting of any kind. Harmost bounds origin *work*, not bytes,
  connections per source, or requests per second.

[0.1.1]: https://github.com/akosidencio/harmost/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/akosidencio/harmost/releases/tag/v0.1.0
