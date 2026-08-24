#!/usr/bin/env bash
# N concurrent requests for the SAME url. The origin counts how many renders it
# was actually asked to perform.
set -uo pipefail
set +m
cd "$(dirname "$0")/.."
CONCURRENCY=${1:-100}
RENDER_MS=${2:-1000}

cargo build --workspace -q || exit 1
cleanup() { pkill -9 -f 'target/debug/slow-origin' 2>/dev/null; pkill -9 -f 'target/debug/harmost run' 2>/dev/null; }
trap cleanup EXIT
cleanup; sleep 1

./target/debug/slow-origin 3000 "$RENDER_MS" >/dev/null 2>&1 & disown
sleep 1
./target/debug/harmost run --config bench/coalesce.yaml >/tmp/harmost.log 2>&1 & disown
sleep 2

echo "$CONCURRENCY concurrent requests for ONE url, ${RENDER_MS}ms render"
echo
OUT=$(seq 1 "$CONCURRENCY" | xargs -P "$CONCURRENCY" -I{} \
  curl -s -o /dev/null -D - --max-time 30 "http://127.0.0.1:8080/product/iphone" 2>/dev/null)

RENDERS=$(echo "$OUT" | grep -i '^x-origin-total:' | tr -dc '0-9\n' | sort -n | tail -1)
STATUSES=$(echo "$OUT" | grep -ci '^HTTP/1.1 200')
echo "  requests served        $STATUSES / $CONCURRENCY"
echo "  origin renders         ${RENDERS:-?}"
echo
echo "  X-Harmost breakdown:"
echo "$OUT" | grep -i '^x-harmost:' | tr -d '\r' | awk '{print "    " $2}' | sort | uniq -c
echo
if [ -n "$RENDERS" ] && [ "$RENDERS" -le 2 ] && [ "$STATUSES" -eq "$CONCURRENCY" ]; then
  echo "PASS: $CONCURRENCY requests collapsed onto ${RENDERS} origin render(s)"
else
  echo "FAIL: served=$STATUSES renders=${RENDERS:-?}"; exit 1
fi
