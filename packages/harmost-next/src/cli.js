#!/usr/bin/env node
import { realpathSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { HarmostNextError } from './manifests.js';
import { generateToFile } from './generate-to-file.js';
import { readBuild } from './manifests.js';
import { generateConfig } from './routes.js';
import { VERIFIED_NEXT_RELEASES } from './compat.js';

const USAGE = `harmost-next — generate Harmost configuration from a Next.js build

USAGE
  harmost-next generate [OPTIONS]

OPTIONS
  --dist-dir <DIR>     Next build output. Default: .next
  --upstream <ADDR>    Where a Next server listens; repeatable. With at least
                       one, the output is a complete config; with none, it is
                       routes only, to paste into an existing file.
  --concurrency <N>    origin.concurrency.max. Default: 200
  --out <FILE>         Write here instead of stdout.
  --routes-only        Omit deployment.id as well as the origin block.
  --check              Run \`harmost check\` on the result and fail if it is
                       rejected. Needs --out and at least one --upstream.
  --harmost-bin <PATH> The harmost binary. Default: $HARMOST_BIN, else PATH.

NOTES
  Every route the build does not prove is shareable is generated private.
  Regenerate after each build: deployment.id is the Next build id, and it is
  what keeps a new build from being served the previous one's cache entries.

  Verified against: ${VERIFIED_NEXT_RELEASES.map((r) => `Next ${r.next}`).join(', ')}
`;

function parseArgs(argv) {
  const options = {
    distDir: '.next',
    upstreams: [],
    concurrency: 200,
    out: null,
    routesOnly: false,
    check: false,
    harmostBin: null,
  };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    const next = () => {
      const value = argv[i + 1];
      if (value === undefined || value.startsWith('--')) {
        throw new HarmostNextError(`${arg} needs a value`);
      }
      i += 1;
      return value;
    };
    switch (arg) {
      case '--dist-dir':
        options.distDir = next();
        break;
      case '--upstream':
        options.upstreams.push(next());
        break;
      case '--concurrency': {
        const value = Number(next());
        if (!Number.isInteger(value) || value <= 0) {
          throw new HarmostNextError('--concurrency must be a positive integer');
        }
        options.concurrency = value;
        break;
      }
      case '--out':
        options.out = next();
        break;
      case '--routes-only':
        options.routesOnly = true;
        break;
      case '--check':
        options.check = true;
        break;
      case '--harmost-bin':
        options.harmostBin = next();
        break;
      default:
        // Refused rather than ignored, on the same reasoning as Harmost's own
        // config: an option that is accepted and does nothing lets somebody
        // believe they configured something.
        throw new HarmostNextError(`unknown option \`${arg}\`\n\n${USAGE}`);
    }
  }
  return options;
}

export async function main(argv) {
  const [command, ...rest] = argv;
  if (!command || command === 'help' || command === '--help' || command === '-h') {
    process.stdout.write(USAGE);
    return 0;
  }
  if (command !== 'generate') {
    process.stderr.write(`harmost-next: unknown command \`${command}\`\n\n${USAGE}`);
    return 2;
  }

  const options = parseArgs(rest);

  if (options.check && !options.out) {
    throw new HarmostNextError('--check needs --out: there is nothing on stdout for Harmost to read');
  }

  if (!options.out) {
    const build = await readBuild(options.distDir);
    process.stdout.write(
      generateConfig(build, {
        upstreams: options.upstreams,
        concurrency: options.concurrency,
        includeDeployment: !options.routesOnly,
      }),
    );
    return 0;
  }

  const result = generateToFile({
    distDir: options.distDir,
    out: options.out,
    upstreams: options.upstreams,
    concurrency: options.concurrency,
    includeDeployment: !options.routesOnly,
    check: options.check,
    harmostBin: options.harmostBin,
  });

  process.stderr.write(
    `harmost-next: wrote ${result.out} — ${result.routes} routes, build ${result.buildId}\n`,
  );
  if (result.checked) {
    process.stderr.write('harmost-next: harmost check passed\n');
  } else if (options.upstreams.length === 0) {
    process.stderr.write(
      'harmost-next: no --upstream given, so this is routes only and not a complete config\n',
    );
  } else {
    process.stderr.write(
      `harmost-next: not checked; add --check, or run \`harmost check --config ${result.out}\`\n`,
    );
  }
  return 0;
}

// Comparing real paths works with both POSIX separators and Windows `\\`.
// A basename suffix can also mistake an unrelated imported `cli.js` for this
// executable.
export function isDirectExecution(moduleFile, argvFile, pathApi = path, canonicalize = realpathSync) {
  if (!argvFile) return false;
  const resolve = (file) => {
    const absolute = pathApi.resolve(file);
    try {
      return canonicalize(absolute);
    } catch {
      return absolute;
    }
  };
  const modulePath = resolve(moduleFile);
  const argumentPath = resolve(argvFile);
  return pathApi.sep === '\\'
    ? modulePath.toLowerCase() === argumentPath.toLowerCase()
    : modulePath === argumentPath;
}

const executedDirectly = isDirectExecution(fileURLToPath(import.meta.url), process.argv[1]);
if (executedDirectly) {
  main(process.argv.slice(2))
    .then((code) => process.exit(code))
    .catch((error) => {
      process.stderr.write(`harmost-next: ${error?.message ?? error}\n`);
      process.exit(1);
    });
}
