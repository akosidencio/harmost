#!/usr/bin/env bash
# The check that matters more than any throughput number.
#
# Same route, same permissive config that collapsed 100 requests into one
# render. But this path answers with Set-Cookie, so every request must get its
# own render — sharing one would hand one visitor's session to ninety-nine
# strangers.
set -uo pipefail
set +m
cd "$(dirname "$0")/.."
CONCURRENCY=${1:-50}

cargo build --workspace -q || exit 1
cleanup() { pkill -9 -f 'target/debug/slow-origin' 2>/dev/null; pkill -9 -f 'target/debug/harmost run' 2>/dev/null; }
trap cleanup EXIT
cleanup; sleep 1

./target/debug/slow-origin 3000 200 >/dev/null 2>&1 & disown
sleep 1
./target/debug/harmost run --config bench/coalesce.yaml >/tmp/harmost.log 2>&1 & disown
sleep 2

echo "$CONCURRENCY concurrent requests to a Set-Cookie route,"
echo "on the same config that collapses 100 requests into 1 render"
echo
OUT=$(seq 1 "$CONCURRENCY" | xargs -P "$CONCURRENCY" -I{} \
  curl -s -o /dev/null -D - --max-time 30 "http://127.0.0.1:8080/private/account" 2>/dev/null)

COOKIES=$(echo "$OUT" | grep -i '^set-cookie:' | sed 's/.*session=//' | tr -d '\r; ' | sort -u | wc -l | tr -d ' ')
SERVED=$(echo "$OUT" | grep -ci '^HTTP/1.1 200')
echo "  requests served        $SERVED / $CONCURRENCY"
echo "  distinct session ids   $COOKIES"
echo
echo "  X-Harmost breakdown:"
echo "$OUT" | grep -i '^x-harmost:' | tr -d '\r' | awk '{print "    " $2}' | sort | uniq -c
echo
if [ "$COOKIES" -eq "$SERVED" ] && [ "$SERVED" -eq "$CONCURRENCY" ]; then
  echo "PASS: every request got its own session; nothing was shared"
else
  echo "FAIL: $SERVED responses carried only $COOKIES distinct sessions — a response was shared"
  exit 1
fi
