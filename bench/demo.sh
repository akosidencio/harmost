#!/usr/bin/env bash
# The headline demonstration: unique URLs, nothing cacheable, nothing to
# coalesce. Only admission control is doing any work.
#
# Fires the same load twice — straight at the origin, then through harmost —
# and reads the peak concurrency the origin itself observed. The origin is the
# witness, so the result does not depend on trusting the proxy's own metrics.
set -uo pipefail
set +m  # keep job-control chatter out of the report
cd "$(dirname "$0")/.."

CONCURRENCY=${1:-100}
RENDER_MS=${2:-1000}
CEILING=$(awk '/^  concurrency:/{f=1} f&&/max:/{print $2; exit}' bench/demo.yaml)

cargo build --workspace -q || exit 1
cleanup() { pkill -9 -f 'target/debug/slow-origin' 2>/dev/null; pkill -9 -f 'target/debug/harmost run' 2>/dev/null; }
trap cleanup EXIT
cleanup; sleep 1

fire() {
  seq 1 "$CONCURRENCY" | xargs -P "$CONCURRENCY" -I{} \
    curl -s -o /dev/null -D - --max-time 25 "$1/product/{}" 2>/dev/null \
    | grep -i '^x-origin-peak:' | tr -dc '0-9\n' | sort -n | tail -1
}

run_case() { # label, url, tag, use_proxy
  ./target/debug/slow-origin 3000 "$RENDER_MS" >/dev/null 2>&1 &
  disown
  sleep 1
  if [ "$4" = "yes" ]; then
    ./target/debug/harmost run --config bench/demo.yaml >/tmp/harmost.log 2>&1 &
    disown
    sleep 2
  fi
  local start peak elapsed
  start=$(date +%s)
  peak=$(fire "$2")
  elapsed=$(( $(date +%s) - start ))
  cleanup; sleep 1
  printf '  %-22s peak=%-6s wall=%ss\n' "$1" "${peak:-?}" "$elapsed"
  echo "${peak:-0}" > "/tmp/peak_$3.txt"
}

echo "$CONCURRENCY concurrent requests, $CONCURRENCY unique URLs, ${RENDER_MS}ms render"
echo "nothing cacheable, nothing coalescible — admission control only"
echo
run_case "direct to origin" "http://127.0.0.1:3000" direct   no
run_case "through harmost"  "http://127.0.0.1:8080" shielded yes
echo
DIRECT=$(cat /tmp/peak_direct.txt); SHIELDED=$(cat /tmp/peak_shielded.txt)
echo "  configured ceiling     $CEILING"
echo "  origin peak, direct    $DIRECT"
echo "  origin peak, harmost   $SHIELDED"
echo
if [ "$SHIELDED" -le "$CEILING" ] && [ "$DIRECT" -gt "$CEILING" ]; then
  echo "PASS: unprotected, the origin was driven to $DIRECT concurrent renders; through harmost, $SHIELDED"
else
  echo "FAIL: direct=$DIRECT shielded=$SHIELDED ceiling=$CEILING"; exit 1
fi
