import type { GenerateOptions } from './index.js';

export interface WithHarmostOptions extends GenerateOptions {
  /** Where to write the generated config. Default `harmost.yaml`. */
  out?: string;
  /** Next build output directory. Default `.next`. */
  distDir?: string;
  /** Run `harmost check` on the result; a rejection fails the build. */
  check?: boolean;
  /** Path to the harmost binary. Default `$HARMOST_BIN`, else `harmost` on PATH. */
  harmostBin?: string;
  /** Suppress the success line. Failures are always reported. */
  silent?: boolean;
}

type NextConfigInput = Record<string, unknown> | ((phase: string, context: unknown) => unknown);

/** Wrap a Next config so a production build also writes Harmost's. */
export function withHarmost(
  nextConfig?: NextConfigInput,
  options?: WithHarmostOptions,
): (phase: string, context: unknown) => unknown;
