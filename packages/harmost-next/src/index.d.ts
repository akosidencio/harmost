/** Everything @harmost/next reads out of a Next.js build. */
export interface NextBuild {
  /** The Next build id, from `.next/BUILD_ID`. Becomes `deployment.id`. */
  buildId: string;
  basePath: string;
  staticRoutes: Array<{ page: string }>;
  dynamicRoutes: Array<{ page: string }>;
  dataRoutes: Array<{ page?: string }>;
  prerendered: Record<string, { initialRevalidateSeconds?: number | false }>;
  appPaths: Record<string, string>;
  manifestVersions: { routes: number; prerender: number | null };
}

export interface GenerateOptions {
  /** Emit a `deployment:` block. Default true. */
  includeDeployment?: boolean;
  /** TTL suggested in the comment for opting a dynamic route into caching. */
  defaultTtl?: string;
  /** `stale_if_error` for prerendered routes. Default `1m`. */
  staleIfError?: string;
  /** Origin addresses. With at least one, a complete config is emitted. */
  upstreams?: string[];
  /** `origin.concurrency.max`. Default 200 — set it from your own measurement. */
  concurrency?: number;
  /** `origin.priorities.low`, which reserves the rest for page renders. */
  lowPriorityPercent?: number;
}

export interface PurgeResult {
  purged: boolean;
  scope?: 'selective' | 'all';
  tags?: number;
  paths?: number;
  entries: number;
  bytes?: number;
  remaining_entries?: number;
}

export interface PurgerOptions {
  /** Harmost's **admin** listener, e.g. `http://127.0.0.1:9091`. */
  endpoint?: string;
  /** Must match `cache.purge.token`. */
  token?: string;
  timeoutMs?: number;
  fetch?: typeof globalThis.fetch;
}

export interface RevalidateTagOptions extends PurgerOptions {
  /** Next cache-life profile. Defaults to `{ expire: 0 }` for immediate invalidation. */
  nextProfile?: string | { expire?: number };
}

export interface Purger {
  purgeTags(tags: readonly string[]): Promise<PurgeResult>;
  purgePaths(paths: readonly string[]): Promise<PurgeResult>;
  purge(what: { tags?: readonly string[]; paths?: readonly string[] }): Promise<PurgeResult>;
  purgeAll(): Promise<PurgeResult>;
}

export class HarmostNextError extends Error {}

export function readBuild(distDir: string): Promise<NextBuild>;
export function generateConfig(build: NextBuild, options?: GenerateOptions): string;
export function toGlob(page: string): string;
export function routeId(page: string, taken: Set<string>): string;
export function createPurger(options?: PurgerOptions): Purger;

/** `revalidateTag()` from `next/cache`, then the same tag in Harmost. */
export function revalidateTag(tag: string, options?: RevalidateTagOptions): Promise<PurgeResult>;
/** `revalidatePath()` from `next/cache`, then the same path in Harmost. */
export function revalidatePath(path: string, options?: PurgerOptions): Promise<PurgeResult>;

export const HARMOST_SCHEMA_VERSION: number;
export const SUPPORTED_MANIFESTS: Readonly<Record<string, readonly number[] | null>>;
export const VERIFIED_NEXT_RELEASES: ReadonlyArray<{
  next: string;
  router: string;
  routesManifest: number;
  prerenderManifest: number;
}>;
