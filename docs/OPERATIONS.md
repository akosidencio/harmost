# Operating Harmost

Everything here is asserted by a script in [`bench/`](../bench) rather than
described from memory. Where a number appears, the benchmark that produced it
is named.

- [The operator surface](#the-operator-surface)
- [Restarting without dropping requests](#restarting-without-dropping-requests)
- [systemd](#systemd)
- [Kubernetes](#kubernetes)
- [Reloading configuration](#reloading-configuration)
- [Tracing and correlation](#tracing-and-correlation)
- [What to alert on](#what-to-alert-on)

---

## The operator surface

Three endpoints on a listener of their own:

```yaml
telemetry:
  admin:
    listen: "127.0.0.1:9091"
    # Report not-ready when no upstream is passing its health check.
    # Off by default — see the warning below.
    require_healthy_upstream: false
```

| Endpoint | Answers |
|---|---|
| `GET /health/live` | `200` while the process is running. Never anything else. |
| `GET /health/ready` | `200` when this instance should receive traffic, `503` while draining (and, if configured, while no upstream is healthy). |
| `GET /status` | Configuration generation and stable fingerprint, per-backend health, cache and spool occupancy, admission limits and in-flight counts, drain state, compiled features. |

`/healthz` and `/readyz` are accepted as aliases, because half the tooling in
the world spells them that way.

**Bind it to loopback or a private address.** `/status` publishes backend
health, cache occupancy and your configuration generation. `harmost check`
warns when it is bound to an unspecified address, and startup refuses to share
an address with the traffic listener or the metrics listener.

Nothing on this surface is parameterised. There is no path segment, query key
or header that changes what is computed — the same rule the Prometheus labels
follow, and for the same reason: an operator surface must never become a way to
make the process do unbounded work during the incident it exists to explain.

**Liveness is not readiness.** A draining instance answers `200` on
`/health/live` and `503` on `/health/ready`. Wiring an orchestrator's liveness
probe to the readiness endpoint makes it kill the process part-way through a
drain, which is precisely the dropped request the drain existed to prevent.

**`require_healthy_upstream` is off by default and the default is the careful
one.** Harmost keeps serving a fully unhealthy pool — refusing to pick a
backend would turn a degraded origin into a guaranteed outage, and
`stale_if_error` exists for that window. Turn it on only when something above
Harmost can route around this instance; otherwise a single origin problem takes
every replica out of rotation at once.

When enabled, it requires a `health:` block and readiness starts at `503`.
Harmost does not call an unprobed backend healthy: `/health/ready` changes to
`200` only after at least one backend completes its configured successful-probe
streak.

Evidence: [`bench/admin.sh`](../bench/admin.sh) and
[`bench/chaos.sh`](../bench/chaos.sh), which samples `/health/live` and
`/status` *during* an origin outage rather than after it.

---

## Restarting without dropping requests

There are two mechanisms and they are not interchangeable.

### Socket handover — Linux only

Pingora passes listening file descriptors between processes over a Unix socket
with `SCM_RIGHTS`. **This works on Linux and nowhere else**: its non-Linux
`get_fds_from` is a stub that logs `Upgrade is not currently supported` and
returns `ECONNREFUSED`. Harmost refuses `--upgrade` up front on other
platforms rather than letting that surface as a connection error that reads
like "the old process is not running".

```bash
# 1. Prove the new binary and config can actually start. Non-zero here means
#    stop: you have not touched the running process yet.
harmost run --config /etc/harmost/harmost.yaml --test

# Obtain this from the supervisor, a captured `$!`, or the pid file written by
# --daemon. Foreground Harmost does not write server.graceful.pid_file.
pid=${HARMOST_PID:?set HARMOST_PID to the running Harmost process}

# 2. Start the new process. It takes the listening sockets over the
#    upgrade socket; both processes must name the same path.
harmost run --config /etc/harmost/harmost.yaml --upgrade &

# 3. Tell the old one to hand over and drain.
kill -QUIT "$pid"
```

The old process keeps serving everything it had already accepted and exits when
those finish. No connection is refused in between — asserted under continuous
traffic by [`bench/upgrade.sh`](../bench/upgrade.sh).

### Drain and replace — everywhere

Without a socket handover there is a window in which nothing owns the port. The
drain window is what lets a load balancer withdraw the instance *before* that
window opens.

```bash
# 1. Start draining. Readiness begins answering 503 immediately; the process
#    keeps serving normally.
kill -USR1 "$pid"

# 2. Wait for your load balancer to notice. Longer than its check interval
#    times its unhealthy threshold.
sleep 15

# 3. Stop, then start the replacement.
kill -TERM "$pid"
```

`SIGUSR1` drains **without exiting**. That is the whole point: the instance
keeps answering requests correctly for as long as you leave it there.

### `shutdown_timeout` is a floor, not a ceiling

The one genuinely surprising thing in this document, and it is measured rather
than assumed — see the `shutdown_seconds` result in
[`bench/upgrade.sh`](../bench/upgrade.sh).

Pingora ends a shutdown with `Runtime::shutdown_timeout` on each service's
runtime and deliberately keeps the final timeout window open. The wait
therefore runs to completion **whether or not anything is still in flight**:

```
time from SIGTERM to exit  ≈  drain_period + shutdown_timeout
```

on a completely idle process. Two things follow:

- **Your supervisor's stop timeout must exceed that sum.** The defaults —
  `drain_period: 5s`, `shutdown_timeout: 10s` — total 15 seconds, which fits
  inside Kubernetes' default `terminationGracePeriodSeconds: 30` and systemd's
  default `TimeoutStopSec=90`. Raise these two and you must raise those, or the
  supervisor `SIGKILL`s Harmost mid-drain.
- **There is no point setting `shutdown_timeout` far above your slowest
  response.** It buys nothing and every restart pays it. `harmost check` prints
  the total and warns above 30 seconds.

If `SIGUSR1` already started draining, the two windows overlap: `SIGTERM` waits
only the unelapsed part of `drain_period`. A pre-stop hook that sleeps longer
than the drain period therefore pays `preStop sleep + shutdown_timeout`, not a
second full drain.

---

## systemd

```ini
[Unit]
Description=Harmost origin workload governor
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=harmost
Group=harmost
RuntimeDirectory=harmost
RuntimeDirectoryMode=0750

# Refuses to start on a bad config, before the old unit is stopped.
ExecStartPre=/usr/local/bin/harmost check --config /etc/harmost/harmost.yaml
ExecStart=/usr/local/bin/harmost run --config /etc/harmost/harmost.yaml

# SIGHUP reloads policy in place. An invalid config is refused and the
# running one keeps serving, so a failed reload is not a failed service.
ExecReload=/bin/kill -HUP $MAINPID

# Must exceed server.graceful.drain_period + shutdown_timeout. See the note
# above: that sum is spent on every stop, idle or not.
KillSignal=SIGTERM
TimeoutStopSec=45
Restart=on-failure
RestartSec=2

# Nothing here needs to write outside its runtime directory.
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/run/harmost
# Binding 80/443 without running as root.
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

with

```yaml
server:
  graceful:
    pid_file: /run/harmost/harmost.pid
    upgrade_socket: /run/harmost/upgrade.sock
```

For a zero-downtime deploy on Linux, do the handover by hand rather than
through `systemctl restart` — systemd stops the unit before starting the new
one, which is the gap the handover exists to remove. Either run the three-step
sequence above against the running unit, or use a socket-activated
`Type=notify` arrangement.

---

## Kubernetes

```yaml
apiVersion: apps/v1
kind: Deployment
spec:
  strategy:
    type: RollingUpdate
    rollingUpdate:
      maxUnavailable: 0
      maxSurge: 1
  template:
    spec:
      # The SIGUSR1 drain overlaps the preStop sleep. Budget the larger of the
      # two, then shutdown_timeout. Here: max(15, 5) + 10 = 25.
      terminationGracePeriodSeconds: 35
      containers:
        - name: harmost
          image: ghcr.io/OWNER/harmost:0.1.1
          args: ["run", "--config", "/etc/harmost/harmost.yaml"]
          ports:
            - { name: http, containerPort: 8080 }
            - { name: admin, containerPort: 9091 }
            - { name: metrics, containerPort: 9090 }

          # Readiness gates Service endpoints. This is what actually stops
          # traffic arriving during a drain.
          readinessProbe:
            httpGet: { path: /health/ready, port: admin }
            periodSeconds: 2
            failureThreshold: 2

          # Liveness must NOT point at /health/ready. A draining pod is alive;
          # killing it here undoes the drain.
          livenessProbe:
            httpGet: { path: /health/live, port: admin }
            periodSeconds: 10
            failureThreshold: 3

          # Kubernetes removes the pod from endpoints and sends SIGTERM at the
          # same time, and endpoint propagation is not instant. The sleep is
          # the window in which kube-proxy on every node catches up; SIGUSR1
          # makes readiness fail immediately so anything that polls directly
          # also stops.
          lifecycle:
            preStop:
              exec:
                command: ["/bin/sh", "-c", "kill -USR1 1; sleep 15"]

          resources:
            requests: { cpu: "500m", memory: "512Mi" }
            # Must exceed cache.max_memory + spool.max_memory plus headroom.
            limits: { memory: "1Gi" }
```

The arithmetic that matters, in one place:

```
terminationGracePeriodSeconds  >  max(preStop sleep, drain_period) + shutdown_timeout
memory limit                   >  cache.max_memory + spool.max_memory + ~128Mi
```

The second is asserted by [`bench/memory.sh`](../bench/memory.sh), which drives
every configured budget past its limit at once and measures resident set size.

`maxUnavailable: 0` matters more than usual here. Each replica has its own
process-local cache and its own admission ceiling, so a replacement pod starts
cold: it has no cached entries and its first requests all reach the origin.
Rolling one pod at a time keeps that surge bounded.

---

## Reloading configuration

`SIGHUP` re-reads the file. **An invalid config is refused and the running one
keeps serving** — reloads happen during incidents, and half-applying one then
is worse than not reloading at all.

Confirm it applied, from outside the process:

```bash
curl -s localhost:9091/status | grep -o '"\(generation\|fingerprint\)":[0-9]*'
# or
curl -s localhost:9090/metrics | grep 'harmost_config_\(generation\|fingerprint\)'
```

A refused reload leaves both values unchanged. Generation distinguishes
requests on either side of a reload within one process; the stable fingerprint
compares effective configuration across replicas regardless of their restart
or reload counts.

Some settings are **startup-bound and a reload refuses rather than ignores
them**: listeners, TLS, `trusted_proxies`, `origin.upstreams`,
`origin.http_version`, `spool.max_memory`, `upgrade.max_concurrent`, the cache
budget, `timeouts.origin`, `server.graceful`, `telemetry.admin`, and
`telemetry.tracing.otlp`. A reload that reported success while the old trust
policy stayed in force would be the worst version of this, so it says so
instead. `telemetry.tracing.sample` and `trust_incoming` **do** reload, because
turning sampling down is something you want to do during an incident.

---

## Tracing and correlation

Correlation is unconditional and costs nothing. Every request gets a W3C trace
id and span id; both appear on every access log line, and the id Harmost
concluded is the one it forwards to the origin as `traceparent`. That is what
joins Harmost's log to the origin's for the same request — even when the origin
has no idea Harmost exists.

Span *export* is configuration:

```yaml
telemetry:
  tracing:
    service_name: harmost
    sample:
      mode: parent_or_ratio   # follow the caller, else sample one in N
      one_in: 20
    otlp:
      endpoint: "http://127.0.0.1:4318/v1/traces"
```

Two spans per sampled request: a server span for what Harmost did, and a client
span for the origin fetch nested under it. The nesting is what lets you tell
"the origin was slow" apart from "we queued for two seconds before asking it".

**An inbound `traceparent` is a claim and is ignored by default.**
`from_trusted_proxies` is safe only when every trusted proxy strips or replaces
client-supplied `traceparent` and `tracestate`. Unlike `X-Forwarded-For`, trace
context has no hop chain Harmost can walk to distinguish an edge-generated
value from one the edge merely forwarded. Ignoring one never costs the request
— Harmost simply starts a fresh trace.

**The exporter is plaintext OTLP/HTTP only.** `https://` endpoints are refused
at startup rather than quietly downgraded. Run an OpenTelemetry Collector as a
sidecar and let it handle transport, authentication and retry.

**Telemetry is never load-bearing.** The span queue is bounded and full means
drop; recording is a non-blocking `try_send`; an export failure is counted and
logged at debug. Final shutdown flushing has one total `otlp.timeout` deadline,
not one timeout per queued batch. A collector that is down costs `harmost_spans_total{outcome=
"export_failed"}` and nothing else — asserted by
[`bench/tracing.sh`](../bench/tracing.sh), which kills the collector and then
measures that fifteen requests still complete promptly.

---

## What to alert on

Rules in [`ops/prometheus/alerts.yml`](../ops/prometheus/alerts.yml), a
dashboard in [`ops/grafana/dashboard.json`](../ops/grafana/dashboard.json). The
four that matter most:

| Signal | Means |
|---|---|
| `harmost_admission_total{decision=~"shed_.*"}` rising | The origin ceiling is being hit. Either the origin got slower or traffic grew. This is Harmost working, but it is also users seeing `503`. |
| `harmost_origin_in_flight` pinned at `harmost_concurrency_limit` | Saturated. Look at `harmost_origin_latency_seconds` before raising the ceiling — a higher ceiling against a slower origin makes it slower still. |
| `harmost_upstream_healthy == 0` | No backend is passing its health check. Harmost is still serving, on `stale_if_error` and on whatever the origin manages. |
| `harmost_draining == 1` for longer than a deploy | An instance drained and was never replaced. It is serving and reporting itself not-ready, so a balancer has withdrawn it and nothing is watching. |

Two that are easy to miss:

- `harmost_spans_total{outcome="dropped"}` — the trace queue is too small for
  the traffic. Costs traces, never requests.
- `harmost_cache_bytes` at `cache.max_memory` **with a low hit ratio** — the
  working set does not fit and eviction is destroying entries before they are
  reused. More memory, or a shorter TTL and a narrower key.
