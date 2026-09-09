#!/usr/bin/env node
import { realpathSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { HarmostNextError } from './manifests.js';
import { generateToFile } from './generate-to-file.js';
import { readBuild } from './manifests.js';
import { readPolicy } from './policy.js';
import { generateConfig } from './routes.js';
import { VERIFIED_NEXT_RELEASES } from './compat.js';
import { calibrate, doctor, explainRequest, formatInspection } from './workflows.js';

const USAGE = `harmost-next — generate Harmost configuration from a Next.js build

USAGE
  harmost-next generate [OPTIONS]
  harmost-next inspect  [--dist-dir DIR] [--policy FILE] [--json]
  harmost-next explain  --url URL [--method METHOD] [--header 'Name: value']
  harmost-next doctor   [--config FILE] [--origin URL] [deployment options]
  harmost-next calibrate --target URL --route PATH --allow-load [load options]

OPTIONS
  --dist-dir <DIR>     Next build output. Default: .next
  --policy <FILE>      Checked-in public-route assertions. Default: none.
  --upstream <ADDR>    Where a Next server listens; repeatable. With at least
                       one, the output is a complete config; with none, it is
                       routes only, to paste into an existing file.
  --concurrency <N>    Required with --upstream. Measure this origin ceiling.
  --rollout <STAGE>    observe, protect, coalesce, or cache. Default: cache.
  --out <FILE>         Write here instead of stdout.
  --identity-out <FILE> Write deployment identity here. With --out, defaults
                       to harmost.deployment.json beside the generated config.
  --routes-only        Omit deployment.id as well as the origin block.
  --check              Run \`harmost check\` on the result and fail if it is
                       rejected. Needs --out and at least one --upstream.
  --harmost-bin <PATH> The harmost binary. Default: $HARMOST_BIN, else PATH.

DOCTOR OPTIONS
  --config <FILE>      Generated Harmost configuration.
  --origin <URL>       Direct origin URL; repeatable.
  --traffic <URL>      Harmost traffic listener.
  --metrics <URL>      Prometheus listener.
  --admin <URL>        Harmost admin listener.
  --purge-token <TOKEN> Verify a scoped no-match purge.
  --doctor-token <TOKEN> Token for internal origin coordination probes.
  --stream-path <PATH> Verify progressive streaming through the traffic URL.
  --public-url <URL>   Expected public host and scheme at the origin.

CALIBRATION OPTIONS
  --target <URL>       Harmost URL to measure.
  --route <PATH>       Explicit route allowlist; repeatable.
  --steps <LIST>       Concurrency steps. Default: 1,2,4,8,16.
  --allow-load         Required acknowledgement that load will be sent.
  --allow-production   Required in addition for a non-local target.

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
    concurrency: null,
    out: null,
    routesOnly: false,
    check: false,
    harmostBin: null,
    policyFile: null,
    identityOut: undefined,
    json: false,
    url: null,
    method: 'GET',
    headers: [],
    config: null,
    origins: [],
    traffic: null,
    metrics: null,
    admin: null,
    purgeToken: null,
    doctorToken: null,
    streamPath: null,
    publicUrl: null,
    target: null,
    routes: [],
    steps: null,
    allowLoad: false,
    allowProduction: false,
    rollout: 'cache',
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
      case '--policy':
        options.policyFile = next();
        break;
      case '--concurrency': {
        const value = Number(next());
        if (!Number.isInteger(value) || value <= 0) {
          throw new HarmostNextError('--concurrency must be a positive integer');
        }
        options.concurrency = value;
        break;
      }
      case '--rollout': {
        const value = next();
        if (!['observe', 'protect', 'coalesce', 'cache'].includes(value)) {
          throw new HarmostNextError('--rollout must be observe, protect, coalesce, or cache');
        }
        options.rollout = value;
        break;
      }
      case '--out':
        options.out = next();
        break;
      case '--identity-out':
        options.identityOut = next();
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
      case '--json':
        options.json = true;
        break;
      case '--url':
      case '--path':
        options.url = next();
        break;
      case '--method':
        options.method = next();
        break;
      case '--header':
        options.headers.push(next());
        break;
      case '--config':
        options.config = next();
        break;
      case '--origin':
        options.origins.push(next());
        break;
      case '--traffic':
        options.traffic = next();
        break;
      case '--metrics':
        options.metrics = next();
        break;
      case '--admin':
        options.admin = next();
        break;
      case '--purge-token':
        options.purgeToken = next();
        break;
      case '--doctor-token':
        options.doctorToken = next();
        break;
      case '--stream-path':
        options.streamPath = next();
        break;
      case '--public-url':
        options.publicUrl = next();
        break;
      case '--target':
        options.target = next();
        break;
      case '--route':
        options.routes.push(next());
        break;
      case '--steps': {
        const value = next();
        options.steps = value.split(',').map(Number);
        break;
      }
      case '--allow-load':
        options.allowLoad = true;
        break;
      case '--allow-production':
        options.allowProduction = true;
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
  if (!['generate', 'inspect', 'explain', 'doctor', 'calibrate'].includes(command)) {
    process.stderr.write(`harmost-next: unknown command \`${command}\`\n\n${USAGE}`);
    return 2;
  }

  const options = parseArgs(rest);

  if (command === 'calibrate') {
    process.stderr.write(
      `harmost-next: calibration target=${options.target ?? '<missing>'} ` +
        `routes=${options.routes.join(',') || '<missing>'} ` +
        `concurrency=${(options.steps ?? [1, 2, 4, 8, 16]).join(',')}\n`,
    );
    const report = await calibrate(options);
    process.stdout.write(`${JSON.stringify(report, null, 2)}\n`);
    return 0;
  }

  const build = await readBuild(options.distDir);
  const policy = options.policyFile ? await readPolicy(options.policyFile, build) : null;

  if (command === 'inspect') {
    process.stdout.write(formatInspection(build, policy, { json: options.json }));
    return 0;
  }

  if (command === 'explain') {
    if (!options.url) throw new HarmostNextError('explain requires --url');
    const result = explainRequest(build, policy, options);
    process.stdout.write(`${JSON.stringify(result, null, 2)}\n`);
    return 0;
  }

  if (command === 'doctor') {
    const result = await doctor(build, options);
    if (options.json) process.stdout.write(`${JSON.stringify(result, null, 2)}\n`);
    else {
      for (const check of result.checks) {
        process.stdout.write(`${check.status.toUpperCase().padEnd(4)} ${check.name}: ${check.detail}\n`);
      }
    }
    return result.ok ? 0 : 1;
  }

  if (options.upstreams.length > 0 && options.concurrency === null) {
    throw new HarmostNextError(
      'a complete configuration requires --concurrency; measure the origin instead of shipping the old 200-request placeholder',
    );
  }

  if (options.check && !options.out) {
    throw new HarmostNextError('--check needs --out: there is nothing on stdout for Harmost to read');
  }

  if (!options.out) {
    process.stdout.write(
      generateConfig(build, {
        upstreams: options.upstreams,
        concurrency: options.concurrency ?? 200,
        includeDeployment: !options.routesOnly,
        policy,
        rollout: options.rollout,
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
    policy,
    identityOut: options.identityOut,
    rollout: options.rollout,
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
  if (result.identityOut) {
    process.stderr.write(`harmost-next: wrote deployment identity ${result.identityOut}\n`);
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
