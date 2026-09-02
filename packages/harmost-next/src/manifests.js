import { readFile } from 'node:fs/promises';
import { readFileSync } from 'node:fs';
import path from 'node:path';

import { SUPPORTED_MANIFESTS } from './compat.js';

export class HarmostNextError extends Error {
  constructor(message, options) {
    super(message, options);
    this.name = 'HarmostNextError';
  }
}

/**
 * Parse one manifest and check its version against what this package knows.
 *
 * Refusing an unknown version is the whole point. The alternative — read it
 * anyway and hope the shape held — produces a route policy derived from a
 * format nobody verified, which is the one thing a cache configuration
 * generator must not do quietly.
 */
function parseManifest(file, name, raw) {
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

function missing(file, cause) {
  return new HarmostNextError(
    `could not read ${file}. Run \`next build\` first, or pass the right --dist-dir.`,
    { cause },
  );
}

/** Assemble the shape the generator consumes. */
function assemble(buildId, routes, prerender, appPaths) {
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

/**
 * Everything the generator reads out of a Next build.
 *
 * `buildId` comes from `BUILD_ID` rather than from a manifest because that is
 * the file Next itself treats as the build's identity, and it is exactly what
 * Harmost's `deployment.id` wants: change it and every cache key changes, so a
 * new build cannot be served the previous build's entries.
 */
export async function readBuild(distDir) {
  const read = async (name, optional = false) => {
    const file = path.join(distDir, name);
    try {
      return parseManifest(file, name, await readFile(file, 'utf8'));
    } catch (cause) {
      if (optional && cause?.code === 'ENOENT') return null;
      if (cause instanceof HarmostNextError) throw cause;
      throw missing(file, cause);
    }
  };

  const idFile = path.join(distDir, 'BUILD_ID');
  const [buildId, routes, prerender, appPaths] = await Promise.all([
    readFile(idFile, 'utf8').then(
      (id) => id.trim(),
      (cause) => {
        throw missing(idFile, cause);
      },
    ),
    read('routes-manifest.json'),
    read('prerender-manifest.json', true),
    read('app-path-routes-manifest.json', true),
  ]);
  return assemble(buildId, routes, prerender, appPaths);
}

/**
 * The same, synchronously.
 *
 * Needed by the `next.config` integration, which does its work in a
 * `process.on('exit')` handler — and an exit handler may only do synchronous
 * work, because the event loop is already gone by the time it runs.
 */
export function readBuildSync(distDir) {
  const read = (name, optional = false) => {
    const file = path.join(distDir, name);
    try {
      return parseManifest(file, name, readFileSync(file, 'utf8'));
    } catch (cause) {
      if (optional && cause?.code === 'ENOENT') return null;
      if (cause instanceof HarmostNextError) throw cause;
      throw missing(file, cause);
    }
  };

  const idFile = path.join(distDir, 'BUILD_ID');
  let buildId;
  try {
    buildId = readFileSync(idFile, 'utf8').trim();
  } catch (cause) {
    throw missing(idFile, cause);
  }
  return assemble(
    buildId,
    read('routes-manifest.json'),
    read('prerender-manifest.json', true),
    read('app-path-routes-manifest.json', true),
  );
}
