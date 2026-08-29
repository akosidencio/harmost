# Configuration schema versioning

Harmost is pre-1.0 and the configuration format will change. This document
says what "will change" is allowed to mean, so that a file written today has a
defined relationship with a binary released later.

**Current schema version: 1.** Required in every file:

```yaml
version: 1
```

A file naming a version this binary does not understand is **refused at
startup**, naming both numbers:

```
error: invalid configuration in /etc/harmost/harmost.yaml
  caused by: config schema version 2 is not supported; this build of harmost
  understands version 1. See docs/CONFIG-SCHEMA.md for the compatibility rules
  and the migration notes
```

Refused, never coerced. A binary that guessed at a version it did not know
would be applying a policy nobody wrote — and in this configuration file, the
policy decides whether a response is shared between users.

`harmost version` prints the schema version a binary speaks, and `/status`
reports it as `config.schema_version`.

---

## The rules

### What may change without a version bump

- **New optional keys.** Anything with a default that reproduces the previous
  behaviour exactly. `telemetry.admin`, `telemetry.tracing` and
  `server.graceful` were all added this way.
- **New enum variants** on a key that already exists, where the existing
  variants keep their meaning.
- **Wider accepted ranges**, where the previously accepted values are unchanged.
- **A key becoming implemented.** Harmost rejects options it accepts but does
  not honour, so a key moving from "rejected as unimplemented" to "works" only
  ever turns a failing config into a working one.

### What requires a version bump

- Removing a key, or renaming one.
- Changing what an existing value **does**, including a changed default that
  alters behaviour.
- Changing a unit, a scale, or the meaning of a bare number.
- Tightening validation so that a previously valid file is now refused.

### What is *never* silent

Two rules, and they are the whole reason this file is short:

1. **An unknown key is an error.** Every struct is `deny_unknown_fields`. A
   typo is a silent policy change otherwise, and in this file a silent policy
   change means an unprotected origin or a cache serving something it should
   not.
2. **An accepted-but-unimplemented key is an error.** If Harmost cannot honour
   a setting, it refuses to start rather than ignoring it. A config that claims
   a protection it is not running is worse than one that does not compile.

Both mean an upgrade can fail loudly. That is the intent: the alternative is an
upgrade that succeeds and quietly does something else.

---

## Migration notes

### → version 1

The first published schema. Nothing to migrate.

Within version 1, these keys have been **added** since the initial release.
All are optional and default to the previous behaviour, so no existing file
needs to change:

| Key | Added with | Default |
|---|---|---|
| `server.h2c` | protocol coverage | `false` |
| `server.tls` | TLS termination | absent |
| `server.trusted_proxies` | forwarded-header trust | trusts nobody |
| `origin.tls`, `origin.http_version` | origin protocol | absent / `http1` |
| `spool.*` | the response spool | disabled |
| `upgrade.*` | WebSocket/Upgrade | disabled, `501` when off |
| `server.graceful.*` | zero-downtime restart | `/tmp` paths, 5s drain, 10s shutdown |
| `telemetry.admin` | readiness and status | absent — **no readiness endpoint** |
| `telemetry.tracing` | OpenTelemetry | correlation on, export off |

Two of those are worth acting on rather than merely noting:

- **`server.graceful.pid_file` and `upgrade_socket` default to `/tmp`.** Two
  Harmost processes on one host with the defaults will hand each other their
  listening sockets. Set them per instance, under `/run` on a systemd host.
- **Without `telemetry.admin` there is no readiness endpoint**, so a load
  balancer cannot tell when an instance is draining and a rolling restart
  drops requests. `harmost check` says so.
- **`telemetry.admin.require_healthy_upstream: true` requires `health:`.** A
  configured backend is unknown until it completes the configured
  `healthy_after` success streak; readiness does not optimistically report it
  healthy during startup.
- **Inbound trace context is ignored by default.** Opt into
  `from_trusted_proxies` only when every trusted proxy strips or replaces
  client-supplied `traceparent` and `tracestate`.

### Keys that are deliberately refused

These parse and are then rejected at startup. Each is listed because someone
will reasonably expect it to work, and silently ignoring it would be the worse
outcome.

| Key | Why |
|---|---|
| `cache.respect_origin: false` | Not implemented. Use per-route `cache.override_origin`, which is fenced by validation. |
| `deployment.id_header` | Impossible by construction: the cache key is built before the response exists. |
| `coalesce.on_timeout: requeue` | Not implemented. |
| `origin.tls.ca` | Pingora 0.8's rustls connector never reads a per-peer CA store. Use `SSL_CERT_FILE` / `SSL_CERT_DIR`. |
| `telemetry.tracing.otlp.endpoint: https://…` | The exporter is plaintext-only by design. Run a collector as a sidecar. |

---

## Checking a file

```bash
harmost check --config /etc/harmost/harmost.yaml
```

Exits non-zero on anything that would prevent a start, and prints the things
that are legal but worth knowing about: a route overriding origin cache
directives, an origin TLS connection that is encrypted but not authenticated,
an admin listener on an unspecified address, and the total time a `SIGTERM`
will take.

`harmost run --config … --test` goes further: it binds every listener and exits
zero only if the process could actually have started. That is the pre-flight
for a zero-downtime upgrade — see [OPERATIONS.md](./OPERATIONS.md).
