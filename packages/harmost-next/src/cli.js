#!/usr/bin/env node
import { writeFile } from 'node:fs/promises';

import { HarmostNextError, readBuild } from './manifests.js';
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
  const build = await readBuild(options.distDir);
  const yaml = generateConfig(build, {
    upstreams: options.upstreams,
    concurrency: options.concurrency,
    includeDeployment: !options.routesOnly,
  });

  if (options.out) {
    await writeFile(options.out, yaml);
    process.stderr.write(
      `harmost-next: wrote ${options.out} (build ${build.buildId})\n` +
        (options.upstreams.length === 0
          ? 'harmost-next: no --upstream given, so this is routes only and not a complete config\n'
          : 'harmost-next: run `harmost check --config ' + options.out + '` before deploying\n'),
    );
  } else {
    process.stdout.write(yaml);
  }
  return 0;
}

// Only when executed, not when imported by a test.
if (process.argv[1] && import.meta.url.endsWith(process.argv[1].split('/').pop())) {
  main(process.argv.slice(2))
    .then((code) => process.exit(code))
    .catch((error) => {
      process.stderr.write(`harmost-next: ${error?.message ?? error}\n`);
      process.exit(1);
    });
}
