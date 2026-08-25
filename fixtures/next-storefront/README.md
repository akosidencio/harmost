# Harmost Next.js storefront fixture

This is a deliberately small but real Next.js App Router application. It uses
standalone output and exercises public SSR, query-keyed dynamic pages, React
Server Component navigation, Suspense streaming, cookies, Draft Mode, a Server
Action, and an absolute `Set-Cookie` privacy barrier.

It is an integration origin, not a benchmark simulator. Each expensive route
emits JSON `render_start` and `render_end` records carrying an instance id and a
unique render id. Harmost's Prometheus origin counter is the machine-readable
witness used by `bench/nextjs.sh` across all three origins.

Run the complete scenario from the repository root:

```bash
./bench/nextjs.sh
```

Or inspect it manually:

```bash
docker compose -f compose.nextjs.yaml up --build
curl -i http://127.0.0.1:18080/products/atlas-runner
```

Only Harmost publishes a host port. The three Next.js services are reachable
only inside the Compose network, matching the intended production topology.
