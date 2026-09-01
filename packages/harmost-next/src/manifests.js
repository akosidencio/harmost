import { readFile } from 'node:fs/promises';
import path from 'node:path';

import { SUPPORTED_MANIFESTS } from './compat.js';

export class HarmostNextError extends Error {
  constructor(message) {
    super(message);
    this.name = 'HarmostNextError';
  }
}

/**
 * Read one manifest and check its version against what this package knows.
 *
 * Refusing an unknown version is the whole point of this function. The
 * alternative — read it anyway and hope the shape held — produces a route
 * policy derived from a format nobody verified, which is the one thing a cache
 * configuration generator must not do quietly.
 */
async function readManifest(distDir, name, { optional = false } = {}) {
  const file = path.join(distDir, name);
  let raw;
  try {
    raw = await readFile(file, 'utf8');
  } catch (cause) {
    if (optional && cause?.code === 'ENOENT') return null;
    throw new HarmostNextError(
      `could not read ${file}. Run \`next build\` first, or pass the right --dist-dir.`,
      { cause },
    );
  }

  let parsed;
  try {
    parsed = JSON.parse(raw);
  } catch (cause) {
    throw new HarmostNextError(`${file} is not valid JSON`, { cause });
  }

  const supported = SUPPORTED_MANIFESTS[name];
  if (supported && !supported.includes(parsed.version)) {
    throw new HarmostNextError(
      `${name} is version ${parsed.version}; @harmost/next understands ` +
        `${supported.join(', ')}. Refusing to generate a route policy from a manifest ` +
        `format nobody has verified — see the compatibility matrix in the README.`,
    );
  }
  return parsed;
}

/**
 * Everything the generator reads out of a Next build.
 *
 * `buildId` is read from `BUILD_ID` rather than from a manifest because that
 * is the file Next itself treats as the build's identity, and it is exactly
 * what Harmost's `deployment.id` wants: change it and every cache key changes,
 * so a new build cannot serve the previous build's entries.
 */
export async function readBuild(distDir) {
  const [buildId, routes, prerender, appPaths] = await Promise.all([
    readFile(path.join(distDir, 'BUILD_ID'), 'utf8').then(
      (id) => id.trim(),
      (cause) => {
        throw new HarmostNextError(
          `could not read ${path.join(distDir, 'BUILD_ID')}. Run \`next build\` first.`,
          { cause },
        );
      },
    ),
    readManifest(distDir, 'routes-manifest.json'),
    readManifest(distDir, 'prerender-manifest.json', { optional: true }),
    readManifest(distDir, 'app-path-routes-manifest.json', { optional: true }),
  ]);

  if (!buildId) {
    throw new HarmostNextError('BUILD_ID is empty; that cannot identify a deployment');
  }

  return {
    buildId,
    basePath: routes.basePath || '',
    staticRoutes: routes.staticRoutes ?? [],
    dynamicRoutes: routes.dynamicRoutes ?? [],
    dataRoutes: routes.dataRoutes ?? [],
    prerendered: prerender?.routes ?? {},
    appPaths: appPaths ?? {},
    manifestVersions: {
      routes: routes.version,
      prerender: prerender?.version ?? null,
    },
  };
}
