#!/usr/bin/env bash
# Can a slow reader occupy origin capacity it is not using?
#
# An origin permit models *render* capacity. Two response shapes behave
# differently and both are checked here:
#
#   buffered  (Content-Length present) — the origin had the whole body before
#             it started writing, so the render is done. The permit is returned
#             at the response header and a slow reader costs nothing.
#
#   streaming (chunked, no Content-Length) — the origin is still producing, and
#             a blocked downstream write really does stall it. Holding the
#             permit is correct here; `timeouts.downstream_write` bounds it.
set -uo pipefail
set +m
cd "$(dirname "$0")/.."
DW=${1:-30s}
cargo build --workspace -q || exit 1
cleanup() { pkill -9 -f 'target/debug/slow-origin' 2>/dev/null; pkill -9 -f 'target/debug/harmost run' 2>/dev/null; }
trap cleanup EXIT

cat > /tmp/slowclient.yaml <<EOF
version: 1
server:
  listen: "127.0.0.1:8080"
origin:
  upstreams: ["127.0.0.1:3000"]
  concurrency:
    max: 2
    queue:
      max: 10
      timeout: 3s
timeouts:
  downstream_write: $DW
cache:
  enabled: false
routes:
  - id: pages
    match: "/**"
    class: public_ssr
EOF

probe() { # $1 = url for the two slow readers, $2 = label
  cleanup; sleep 1
  ./target/debug/slow-origin 3000 50 >/dev/null 2>&1 & disown
  sleep 1
  ./target/debug/harmost run --config /tmp/slowclient.yaml >/tmp/h.log 2>&1 & disown
  sleep 2
  curl -s -o /dev/null --limit-rate 32k "$1" & disown
  curl -s -o /dev/null --limit-rate 32k "$1" & disown
  sleep 3
  local start code ms
  start=$(date +%s%N)
  code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 "http://127.0.0.1:8080/quick")
  ms=$(( ($(date +%s%N) - start) / 1000000 ))
  if [ "$code" = "200" ] && [ "$ms" -lt 1000 ]; then RESULT=yes; else RESULT=no; fi
  printf '  %-28s %-8s %-9s %s\n' "$2" "$code" "${ms}ms" "$RESULT"
}

echo "ceiling = 2, two readers at 32KB/s, then one normal request"
echo "timeouts.downstream_write = $DW"
echo
printf '  %-28s %-8s %-9s %s\n' "response" "status" "latency" "capacity returned?"

FAIL=0
for MIB in 1 4 16; do
  probe "http://127.0.0.1:8080/big/x/$MIB" "buffered ${MIB}MiB"
  [ "$RESULT" = "yes" ] || FAIL=1
done
probe "http://127.0.0.1:8080/bigstream/40" "streaming (chunked)"
STREAM_RESULT=$RESULT

echo
if [ "$FAIL" = "0" ]; then
  echo "PASS: buffered responses return render capacity at the response header,"
  echo "      so a slow reader no longer occupies a render slot at any size."
else
  echo "FAIL: a slow reader is holding a permit on a buffered response."
  exit 1
fi
echo
echo "Streaming, by contrast: capacity returned = $STREAM_RESULT."
echo "That is expected — a chunked response means the origin is still rendering,"
echo "so the permit is still buying something. Lower timeouts.downstream_write"
echo "if you serve long streams to untrusted clients."
