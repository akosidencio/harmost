#!/usr/bin/env bash
# Does request collapsing destroy streaming?
#
# A leader that streams a shell immediately and finishes 1.6s later must not
# make every waiter wait 1.6s for its first byte. If waiters only get the
# response once the leader completes, coalescing turns a 1ms TTFB into a 1.6s
# one for everybody but the leader — the component that exists to make things
# faster making them dramatically slower.
set -uo pipefail
set +m
cd "$(dirname "$0")/.."
CONCURRENCY=${1:-20}
CHUNKS=${2:-5}
RENDER_MS=${3:-400}

cargo build --workspace -q || exit 1
cleanup() { pkill -9 -f 'target/debug/slow-origin' 2>/dev/null; pkill -9 -f 'target/debug/harmost run' 2>/dev/null; }
trap cleanup EXIT
cleanup; sleep 1

./target/debug/slow-origin 3000 "$RENDER_MS" >/dev/null 2>&1 & disown
sleep 1
./target/debug/harmost run --config bench/coalesce.yaml >/tmp/h.log 2>&1 & disown
sleep 2

echo "$CONCURRENCY concurrent requests for one streaming url"
echo "($CHUNKS chunks, ${RENDER_MS}ms apart — the origin takes ~$(( (CHUNKS-1) * RENDER_MS ))ms to finish)"
echo

OUT=$(seq 1 "$CONCURRENCY" | xargs -P "$CONCURRENCY" -I{} \
  curl -s -o /dev/null --max-time 30 \
  -w '%{time_starttransfer} %{time_total} %{http_code}\n' \
  "http://127.0.0.1:8080/stream/$CHUNKS" 2>/dev/null)

RENDERS=$(grep -c 'X-Origin-Total' /dev/null 2>/dev/null || true)
RENDERS=$(grep -o '"upstream":"127' /tmp/h.log | wc -l | tr -d ' ')
SERVED=$(echo "$OUT" | grep -c ' 200$')

echo "$OUT" | sort -n | awk -v n="$SERVED" '
  { ttfb[NR]=$1; total[NR]=$2; s_ttfb+=$1; s_total+=$2 }
  END {
    printf "  requests served       %d\n", NR
    printf "  median TTFB           %.3fs\n", ttfb[int(NR/2)+1]
    printf "  max TTFB              %.3fs\n", ttfb[NR]
    printf "  median total          %.3fs\n", total[int(NR/2)+1]
  }'
echo "  origin requests       $RENDERS"
echo

MAXTTFB=$(echo "$OUT" | awk '{print $1}' | sort -n | tail -1)
MEDTOTAL=$(echo "$OUT" | awk '{print $2}' | sort -n | awk '{a[NR]=$1} END{print a[int(NR/2)+1]}')
STREAMED=$(awk -v t="$MAXTTFB" -v c="$MEDTOTAL" 'BEGIN{print (t < c/2) ? 1 : 0}')

# Both halves must hold. Checking only TTFB would pass trivially when no
# collapsing happened at all, because every request then has its own origin
# connection and of course gets the shell immediately.
FAIL=0
if [ "$RENDERS" -gt 2 ]; then
  echo "FAIL: $RENDERS origin renders — requests were not collapsed"
  FAIL=1
fi
if [ "$STREAMED" != "1" ]; then
  echo "FAIL: max TTFB ${MAXTTFB}s against a median total of ${MEDTOTAL}s —"
  echo "      waiters were served only after the leader finished; coalescing buffered the response"
  FAIL=1
fi
if [ "$FAIL" = "0" ]; then
  echo "PASS: $SERVED requests, $RENDERS origin render — waiters received the shell"
  echo "      while the leader was still rendering (max TTFB ${MAXTTFB}s of ${MEDTOTAL}s)"
else
  exit 1
fi
