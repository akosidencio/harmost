import { execFileSync } from 'node:child_process';
import { renameSync, unlinkSync, writeFileSync } from 'node:fs';
import { randomUUID } from 'node:crypto';
import path from 'node:path';

import { HarmostNextError, readBuildSync } from './manifests.js';
import { generateConfig } from './routes.js';

/**
 * Where to find the `harmost` binary.
 *
 * Explicit argument, then `HARMOST_BIN`, then whatever is on `PATH`. The last
 * one is why `--check` can be the default in a Dockerfile that has the binary
 * and simply omitted anywhere that does not.
 */
export function harmostBin(explicit) {
  return explicit || process.env.HARMOST_BIN || 'harmost';
}

/**
 * Generate the config, write it, and optionally have Harmost validate it.
 *
 * One function rather than two so the CLI and the `next.config` integration
 * cannot drift into doing subtly different things — a generated file that is
 * checked in CI and unchecked locally is how a config that only fails in
 * production gets written.
 *
 * Synchronous throughout: the `next.config` path runs inside a
 * `process.on('exit')` handler, where nothing asynchronous can complete.
 */
export function generateToFile(options = {}) {
  const {
    distDir = '.next',
    out,
    upstreams = [],
    concurrency = 200,
    includeDeployment = true,
    check = false,
    harmostBin: bin,
  } = options;

  if (!out) throw new HarmostNextError('generateToFile needs an `out` path');

  const build = readBuildSync(distDir);
  const yaml = generateConfig(build, { upstreams, concurrency, includeDeployment });
  const result = { buildId: build.buildId, out, routes: countRoutes(yaml), checked: false };

  if (check && upstreams.length === 0) {
    throw new HarmostNextError(
      'cannot check a routes-only fragment: it has no `origin:` block, so Harmost will ' +
        'refuse it. Pass upstreams, or turn the check off.',
    );
  }

  const temporary = path.join(
    path.dirname(out),
    `.${path.basename(out)}.${process.pid}.${randomUUID()}.tmp`,
  );
  let committed = false;
  try {
    // A sibling temporary file keeps validation from destroying the last
    // known-good config and makes the final replacement atomic.
    writeFileSync(temporary, yaml, { flag: 'wx' });

    if (check) {
      const binary = harmostBin(bin);
      try {
        execFileSync(binary, ['check', '--config', temporary], {
          stdio: 'pipe',
          encoding: 'utf8',
        });
      } catch (cause) {
        if (cause?.code === 'ENOENT') {
          throw new HarmostNextError(
            `\`${binary}\` is not on PATH, so the generated config could not be checked. ` +
              'Set HARMOST_BIN, pass --harmost-bin, or turn the check off where the binary ' +
              'is not available.',
            { cause },
          );
        }
        const detail = `${cause?.stdout ?? ''}${cause?.stderr ?? ''}`.trim();
        throw new HarmostNextError(
          `\`${binary} check\` rejected the generated configuration:\n${detail}`,
          { cause },
        );
      }
      result.checked = true;
    }

    renameSync(temporary, out);
    committed = true;
    return result;
  } finally {
    if (!committed) {
      try {
        unlinkSync(temporary);
      } catch {
        // Preserve the generation or validation error that brought us here;
        // a cleanup failure must not replace its actionable diagnostics. The
        // uniquely named file cannot be mistaken for the committed config and
        // a later run will never reuse it.
      }
    }
  }
}

function countRoutes(yaml) {
  return [...yaml.matchAll(/^  - id: "/gm)].length;
}
