import { HarmostNextError } from './manifests.js';

function assertValue(kind, value) {
  if (typeof value !== 'string' || value.length === 0) {
    throw new HarmostNextError(`${kind} must be a non-empty string`);
  }
}

/**
 * A tag may not contain a comma, because one never could have been stored: the
 * tag header is comma-separated, so Harmost split it before indexing. Purging
 * such a tag is guaranteed to match nothing, which is exactly the silent
 * no-op this package refuses everywhere else.
 */
function assertTag(tag) {
  assertValue('tag', tag);
  if (tag.includes(',')) {
    throw new HarmostNextError(
      `tag ${JSON.stringify(tag)} contains a comma. The tag header is comma-separated, so a ` +
        'tag with one in it was never stored under that name and purging it would match nothing.',
    );
  }
}

function assertPath(path) {
  assertValue('path', path);
  if (!path.startsWith('/')) {
    throw new HarmostNextError(
      `path ${JSON.stringify(path)} must be absolute; Harmost stores the request path, which ` +
        'always begins with "/"',
    );
  }
}

/**
 * A client for Harmost's purge endpoint.
 *
 * `endpoint` is the **admin** listener, not the traffic one. It is usually
 * loopback or a private address, which is where the token's transport
 * protection comes from — the admin listener does not speak TLS.
 */
export function createPurger(options = {}) {
  const {
    endpoint = process.env.HARMOST_PURGE_URL,
    token = process.env.HARMOST_PURGE_TOKEN,
    timeoutMs = 2000,
    fetch: fetchImpl = globalThis.fetch,
  } = options;

  if (!endpoint) {
    throw new HarmostNextError(
      'no Harmost endpoint: pass `endpoint` or set HARMOST_PURGE_URL to the admin listener, ' +
        'for example http://127.0.0.1:9091',
    );
  }
  if (!token) {
    throw new HarmostNextError(
      'no purge token: pass `token` or set HARMOST_PURGE_TOKEN. It must match ' +
        'cache.purge.token, and without one the endpoint does not exist.',
    );
  }
  if (typeof fetchImpl !== 'function') {
    throw new HarmostNextError('no fetch available; pass one explicitly');
  }

  const base = new URL('/purge', endpoint);

  async function send(params, description) {
    const url = new URL(base);
    // Harmost percent-decodes every value exactly once. URLSearchParams keeps
    // delimiters inside tags and paths from changing the shape of the query.
    // It emits form-style `+` for spaces, but Harmost deliberately implements
    // percent decoding rather than form decoding, so use `%20` instead.
    url.search = new URLSearchParams(params).toString().replaceAll('+', '%20');

    let response;
    try {
      response = await fetchImpl(url, {
        method: 'POST',
        headers: {
          // In a header, never in the URL: query strings are logged by
          // everything on the path.
          authorization: `Bearer ${token}`,
          accept: 'application/json',
        },
        // A redirect would re-send the Authorization header to whatever host
        // the redirect names. The purge endpoint never redirects, so any
        // redirect here is something to refuse rather than follow.
        redirect: 'manual',
        signal: AbortSignal.timeout(timeoutMs),
      });
    } catch (cause) {
      throw new HarmostNextError(
        `purge request to ${url.origin} failed: ${cause?.message ?? cause}`,
        { cause },
      );
    }

    if (response.status >= 300 && response.status < 400) {
      throw new HarmostNextError(
        `purge endpoint answered a ${response.status} redirect; refusing to re-send the token ` +
          'to another host',
      );
    }
    if (!response.ok) {
      const detail = await response.text().catch(() => '');
      throw new HarmostNextError(
        `purge (${description}) failed: HTTP ${response.status} ${detail.trim()}`.trim(),
      );
    }
    let result;
    try {
      result = await response.json();
    } catch (cause) {
      throw new HarmostNextError(
        `purge (${description}) returned HTTP ${response.status} with invalid JSON`,
        { cause },
      );
    }
    if (
      !result ||
      typeof result !== 'object' ||
      Array.isArray(result) ||
      result.purged !== true ||
      !Number.isSafeInteger(result.entries) ||
      result.entries < 0
    ) {
      throw new HarmostNextError(
        `purge (${description}) returned HTTP ${response.status} with an invalid success body`,
      );
    }
    return result;
  }

  return {
    /** Invalidate every entry carrying any of these tags. */
    async purgeTags(tags) {
      const list = [...new Set(tags)].filter(Boolean);
      list.forEach(assertTag);
      if (list.length === 0) return { purged: false, entries: 0 };
      return send(
        list.map((tag) => ['tag', tag]),
        `${list.length} tag(s)`,
      );
    },

    /** Invalidate every variant of each of these exact paths. */
    async purgePaths(paths) {
      const list = [...new Set(paths)].filter(Boolean);
      list.forEach(assertPath);
      if (list.length === 0) return { purged: false, entries: 0 };
      return send(
        list.map((path) => ['path', path]),
        `${list.length} path(s)`,
      );
    },

    /** Both at once, in a single request. */
    async purge({ tags = [], paths = [] } = {}) {
      const tagList = [...new Set(tags)].filter(Boolean);
      const pathList = [...new Set(paths)].filter(Boolean);
      tagList.forEach(assertTag);
      pathList.forEach(assertPath);
      if (tagList.length === 0 && pathList.length === 0) {
        throw new HarmostNextError('purge needs at least one tag or path');
      }
      return send(
        [
          ...tagList.map((tag) => ['tag', tag]),
          ...pathList.map((path) => ['path', path]),
        ],
        `${tagList.length} tag(s), ${pathList.length} path(s)`,
      );
    },

    /**
     * Invalidate everything.
     *
     * Expect an origin load spike proportional to your traffic — every cached
     * page re-renders on its next request. This is a deploy-time or
     * incident-time tool, not a routine one.
     */
    async purgeAll() {
      return send([['all', '1']], 'everything');
    },
  };
}

/**
 * `revalidateTag()` from `next/cache`, and then the same tag in Harmost.
 *
 * Both are needed and neither is redundant: Next invalidates its own
 * incremental cache inside the server, Harmost invalidates the shared copy in
 * front of it. Doing only the first leaves Harmost serving the old page until
 * its TTL expires, which is the failure this function exists to prevent.
 *
 * Harmost's purge is attempted **after** Next's, and a failure throws. Stale
 * content served silently is worse than a failed deploy hook: one is visible
 * immediately, the other is discovered by a customer.
 */
export async function revalidateTag(tag, options = {}) {
  const { nextProfile = { expire: 0 }, ...purgerOptions } = options;
  const { revalidateTag: nextRevalidateTag } = await importNextCache();
  // Immediate expiry is deliberate. With stale-while-revalidate, the first
  // Harmost miss could fetch stale HTML from Next and cache it again.
  nextRevalidateTag(tag, nextProfile);
  return createPurger(purgerOptions).purgeTags([tag]);
}

/** The same, for `revalidatePath()`. */
export async function revalidatePath(path, options) {
  const { revalidatePath: nextRevalidatePath } = await importNextCache();
  nextRevalidatePath(path);
  return createPurger(options).purgePaths([path]);
}

async function importNextCache() {
  try {
    // @ts-ignore -- `next` is an optional peer dependency, so this module is
    // resolvable in a Next app and absent everywhere else. That is exactly why
    // the import is dynamic and inside a try.
    return await import('next/cache');
  } catch (cause) {
    throw new HarmostNextError(
      'could not import `next/cache`. The revalidate helpers only work inside a Next.js ' +
        'server; from anywhere else use createPurger().purgeTags() / .purgePaths() directly.',
      { cause },
    );
  }
}
