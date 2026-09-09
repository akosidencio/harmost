# Next.js production reference

This reference runs an edge proxy in front of one Harmost governor and three
identical standalone Next.js origins. The origins share Next.js cache and tag
state through a mounted cache directory. Only the edge listener is public; the
Harmost admin and metrics listeners and direct origin ports bind to loopback.

## Build and start

```bash
cargo build --locked
docker compose -f compose.nextjs.yaml up --build -d
```

All origins use one image, one `deploymentId`, and one Server Action encryption
key. The runtime image contains `public`, `.next/static`, and a deployment
identity artifact. Do not rebuild origins separately during a rollout.

The shared-filesystem cache handler is a reference implementation for a
filesystem with atomic rename and coherent reads. Replace the mounted volume
with durable shared storage before running the origins on different hosts.

## Approve routes

Public dynamic routes live in
[`fixtures/next-storefront/harmost.next.yaml`](../fixtures/next-storefront/harmost.next.yaml).
Anything absent stays private.

```bash
node packages/harmost-next/src/cli.js inspect \
  --dist-dir fixtures/next-storefront/.next \
  --policy fixtures/next-storefront/harmost.next.yaml

node packages/harmost-next/src/cli.js generate \
  --dist-dir fixtures/next-storefront/.next \
  --policy fixtures/next-storefront/harmost.next.yaml \
  --upstream next-1:3000 \
  --upstream next-2:3000 \
  --upstream next-3:3000 \
  --concurrency 8 \
  --out /tmp/harmost.yaml \
  --check
```

Generation writes `harmost.deployment.json` beside the configuration. Commit
the assertion file; treat generated configuration and identity files as build
artifacts.

## Roll out in stages

Generate each stage with `--rollout <stage>`, validate it, then reload Harmost.
Move to the next stage only when the dashboard matches the expected behavior.

| Stage | Behavior | Expected signal | Rollback |
| --- | --- | --- | --- |
| `observe` | Classification and telemetry only | Origin traffic unchanged; admission decision is `observe` | Route ingress around Harmost |
| `protect` | Fixed admission and bounded queues | In-flight work stays below the ceiling; no cache hits | Reload `observe` configuration |
| `coalesce` | Public requests may share one in-flight render | Origin requests fall during identical bursts; cache stays empty | Reload `protect` configuration |
| `cache` | Short caching for approved routes | Hits rise and origin work falls; invalidation remains scoped | Reload `coalesce` configuration |
| replicate | Add Harmost replicas after Phase 6 capacity tests | Group capacity remains within its declared budget | Return ingress to one Harmost replica |

Every generated stage keeps the same route privacy decisions. The `observe`
mode disables admission, cache, coalescing, and spooling in the proxy itself,
so route-level settings cannot accidentally turn protection on.

## Diagnose and calibrate

Explain a request locally without contacting an origin:

```bash
node packages/harmost-next/src/cli.js explain \
  --dist-dir fixtures/next-storefront/.next \
  --policy fixtures/next-storefront/harmost.next.yaml \
  --url 'https://shop.example/products/one?ref=email' \
  --header 'Cookie: session=redacted'
```

Run all bounded reference checks:

```bash
./bench/nextjs-reference.sh
```

Calibration requires an explicit load acknowledgement and route allowlist. It
prints the target, routes, and concurrency steps before sending requests and
never edits the configuration.

```bash
node packages/harmost-next/src/cli.js calibrate \
  --target http://127.0.0.1:18080 \
  --route /products/calibration \
  --steps 1,2,4,8 \
  --allow-load
```

Use the recommendation as a starting point for review. Repeat the measurement
on production hardware with representative renders and monitoring before
raising the fixed ceiling.

## Verify

The HTTP and browser suites cover public/private isolation, RSC variants,
Server Actions, Draft Mode, streaming, image negotiation, and coalescing. The
reference suite additionally checks build identity across origins, shared
Next.js cache invalidation, forwarded public URL, scoped Harmost purging,
metrics, and progressive streaming.

```bash
./bench/nextjs.sh
./bench/nextjs-browser.sh
./bench/nextjs-reference.sh
```

Production sign-off still requires an independent cache-safety review, an
artifact/rollback exercise, staging validation, and a sustained deployment
report. Those are evidence from real operation rather than repository features.
