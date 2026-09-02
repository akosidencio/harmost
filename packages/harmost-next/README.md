# @harmost/next

Two things Harmost cannot work out on its own, taken from the place that
already knows them — your Next.js build.

1. **Generate route configuration.** `next build` writes manifests saying which
   routes exist, which are prerendered, which are Route Handlers, and what the
   build id is. This turns that into a Harmost config, so route policy is
   derived from the build rather than guessed at by hand.
2. **Route invalidation to Harmost.** `revalidateTag()` and `revalidatePath()`
   invalidate Next's own cache. They do nothing to the copy Harmost is serving
   in front of it. These helpers do both.

Zero runtime dependencies. Runs on Node and Bun.

Written in JavaScript with no build step — the published files are the files
that run — and type-checked as if it were TypeScript: `npm run typecheck` runs
`tsc` over the source with `checkJs`, and consumers get full types from
`index.d.ts`.

```bash
npm install --save-dev @harmost/next   # or: bun add -d @harmost/next
```

---

## Generating configuration

```bash
next build
npx harmost-next generate --upstream next-1:3000 --out harmost.yaml --check
```

`--check` runs `harmost check` on a temporary sibling file and **fails if it is
rejected**. The previous config is replaced atomically only after validation,
so a failed generation cannot destroy the last known-good output.

Regenerate after every build. `deployment.id` is the Next build id and it is
part of every cache key, so a new build is never served the previous build's
entries — and the `SIGHUP` that applies the new id purges them.

### What it generates, and what it refuses to

**Anything the build does not prove is shareable is generated private.**

A prerendered route is proof: Next produced one response for everybody, so it
can be given to everybody. That is the only evidence in the build, and it is
the only thing that earns a public class — with `override_origin`, which is
correct here precisely because the build is the evidence.

A dynamically rendered route is not proof of anything. It may read cookies, a
session, or a header. So it is generated as `private_dynamic` with a comment
saying exactly how to opt in. A generator that guessed `public_ssr` would be
one bad guess away from serving one user's page to another, and it would do it
silently, in a file nobody reads closely because a tool wrote it.

It also generates the two routes that are easy to get wrong by hand:

- `/_next/static/**` as `static` — serving a chunk is not rendering.
- `/_next/image` with `vary: [Accept]`, which is load-bearing. Next negotiates
  the output format on `Accept` and answers `Vary: Accept`, so without it in
  the key Harmost refuses to store the response and the route gets a 0% hit
  rate. It also gets `priority: low` and `weight: 4`, because an image
  transform is several hundred milliseconds of origin CPU and must not starve
  page renders.

### Running it automatically

Two ways, and they are for different jobs.

**A `postbuild` script — the one to use in CI.** `npm run build` and
`bun run build` both run it, its exit code is the build's exit code, and it
appears in the log as its own step:

```json
{
  "scripts": {
    "build": "next build",
    "postbuild": "harmost-next generate --upstream next-1:3000 --out harmost.yaml --check"
  }
}
```

**A `next.config` wrapper — for local development**, so nobody has to remember
a second command:

```js
// next.config.mjs
import { withHarmost } from '@harmost/next/config';

export default withHarmost(
  { output: 'standalone' },
  { out: 'harmost.yaml', upstreams: ['next-1:3000'], check: true },
);
```

Next has no post-build callback, so the wrapper works from a
`process.on('exit')` handler. That is worth knowing rather than hiding:

- **It only runs on a successful build.** A non-zero exit means the build
  failed, and generating a route policy from a half-finished build would leave
  a stale file that looks current.
- **It can still fail the build.** Setting the exit code from an exit handler
  works, so a config Harmost rejects fails the build that produced it — not a
  warning nobody reads.
- **It fires once**, guarded by an environment variable, because Next loads
  `next.config` in its compilation workers as well as in the build itself.
- **It does nothing in `next dev`**, because it is gated on the
  production-build phase.

Both paths call the same code, so a config that is checked in CI and unchecked
locally is not a thing that can happen.

Where the `harmost` binary is not available — a build container that does not
ship it — omit `--check` rather than letting it fail; the generate step needs
nothing but Node.

### Options

```
--dist-dir <DIR>     Next build output. Default: .next
--upstream <ADDR>    Repeatable. With at least one, the output is a complete
                     config; with none, it is routes only.
--concurrency <N>    origin.concurrency.max. Default: 200
--out <FILE>         Write here instead of stdout.
--routes-only        Omit deployment.id as well as the origin block.
--check              Run `harmost check` on the result and fail if it is
                     rejected. Needs --out and at least one --upstream.
--harmost-bin <PATH> The harmost binary. Default: $HARMOST_BIN, else PATH.
```

`concurrency` is the one number that has to come from your own measurement: it
is the ceiling on how much work the origin does at once, and the right value is
a property of your renders and your hardware, not of your framework.

---

## Invalidation

Set `cache.purge.token` and a `telemetry.admin` listener in Harmost, then:

```js
import { revalidateTag, revalidatePath } from '@harmost/next';

// Invalidates Next's incremental cache AND Harmost's copy.
await revalidateTag('product-42');
await revalidatePath('/products/iphone');
```

Tag invalidation expires Next immediately with `{ expire: 0 }` by default.
This prevents the first Harmost miss from receiving stale-while-revalidate
HTML from Next and caching it again. To select another Next cache-life profile,
pass `nextProfile` alongside the Harmost connection options:

```js
await revalidateTag('product-42', { nextProfile: 'max' });
```

Or without Next in the loop — from a deploy hook, a CLI, a webhook:

```js
import { createPurger } from '@harmost/next';

const harmost = createPurger({
  endpoint: process.env.HARMOST_PURGE_URL,   // the ADMIN listener
  token: process.env.HARMOST_PURGE_TOKEN,
});

await harmost.purge({ tags: ['sale'], paths: ['/products/iphone'] });
```

Four behaviours worth knowing, all of them deliberate:

- **A failed purge throws.** That includes a `2xx` response whose JSON does not
  prove the purge succeeded, which catches traffic listeners and proxies that
  answer the purge path with an HTML success page.
- **Values are encoded exactly once.** Harmost percent-decodes purge parameters
  once, so spaces, `%`, `&`, `?`, and `#` remain part of the tag or path rather
  than becoming query syntax. A comma in a tag is still refused because the
  origin tag header is comma-separated and could never have stored that name.
- **The token travels in an `Authorization` header, never in the URL**, and
  redirects are refused rather than followed, so it cannot be bounced to
  another host.
- **An empty list is a no-op, not a purge of everything.** `purgeAll()` is
  spelled out because it makes every cached page re-render.

---

## Compatibility

Next's build manifests are internal formats with their own version numbers,
independent of Next's release version. **They are the compatibility surface.**
An unknown manifest version is refused rather than guessed at, for the same
reason Harmost refuses an unknown config schema: generating a route policy from
a format nobody verified is how a cache ends up sharing what it should not.

| Next | Router | `routes-manifest` | `prerender-manifest` | Status |
|---|---|---|---|---|
| 16.3.3 | App + Pages | v3 | v4 | Verified |

"Verified" means a real `next build` output was read by the generator and the
result passed `harmost check`. The table is asserted against that build in the
test suite, so it cannot quietly go stale.

Other Next releases may well work — the manifest versions are what matter — but
nobody has run them. If yours is refused, the message names both versions.

### Runtimes

| Runtime | Status |
|---|---|
| Node 22.22 | Verified — `npm test` |
| Bun 1.4 | Verified — `npm run test:bun` |

`npm run test:all` runs both.

One suite, both runtimes: the package uses only `node:` builtins and web
standards (`fetch`, `AbortSignal`), and the tests are `node:test`, which Bun
runs natively.

---

## What this does not do

- **No `revalidatePath()` route patterns.** Harmost purges by exact path, so
  `revalidatePath('/products/[slug]', 'page')` has no equivalent; matching a
  dynamic route pattern needs route metadata Harmost does not carry.
- **No fan-out.** Harmost's cache is per process, so a purge reaches one
  instance. Call every replica, or accept that invalidation is eventually
  consistent within one TTL.
- **No route cost hints.** `weight` is generated for the image route only.
  Nothing in a Next build says what a page costs to render.
