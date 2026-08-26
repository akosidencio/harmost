#!/usr/bin/env bash
# Run every local benchmark and report which ones held.
#
# Each script asserts its own claim and exits non-zero when it fails, so this
# is a gate, not a summary. Set BENCH_REPORT_DIR to collect one JSON file per
# benchmark, each carrying the parameters that produced its numbers — CI does
# this and publishes the directory as an artifact.
set -uo pipefail
cd "$(dirname "$0")/.."

export BENCH_REPORT_DIR=${BENCH_REPORT_DIR:-}
FAILED=""
PASSED=""

run() { # script, args...
  local script=$1; shift
  echo
  echo "=================================================================="
  echo "  $script $*"
  echo "=================================================================="
  if "./bench/$script" "$@"; then
    PASSED="$PASSED $script"
  else
    FAILED="$FAILED $script"
  fi
}

# Concurrency is kept modest so a two-core CI runner still produces genuine
# overlap. The headline numbers in the README come from the larger parameters
# printed in each script's own parameter block.
run demo.sh "${DEMO_CONCURRENCY:-40}" "${DEMO_RENDER_MS:-800}"
run coalesce.sh "${COALESCE_CONCURRENCY:-40}" "${COALESCE_RENDER_MS:-800}"
run stream.sh "${STREAM_CONCURRENCY:-20}" 5 400
run safety.sh "${SAFETY_CONCURRENCY:-30}"
run reload.sh
run slowclient.sh

# The Next.js proof needs Docker. It is skipped loudly rather than quietly: a
# suite that reports success while silently omitting its only real-framework
# test is exactly the kind of evidence this phase exists to remove.
if [ "${SKIP_DOCKER:-0}" = "1" ]; then
  echo
  echo "SKIPPED bench/nextjs.sh (SKIP_DOCKER=1)"
elif docker info >/dev/null 2>&1; then
  run nextjs.sh
else
  echo
  echo "SKIPPED bench/nextjs.sh — no Docker daemon reachable."
  echo "  The real Next.js integration proof did NOT run. Set SKIP_DOCKER=1 to"
  echo "  acknowledge this deliberately."
  FAILED="$FAILED nextjs.sh(no-docker)"
fi

echo
echo "=================================================================="
[ -n "$PASSED" ] && echo "passed:$PASSED"
if [ -n "$FAILED" ]; then
  echo "FAILED:$FAILED"
  exit 1
fi
echo "all benchmarks held"
