#!/usr/bin/env bash
# Characterises a KNOWN GAP, and does not gate anything.
#
# An origin permit represents render capacity, so it should be returned when
# the origin stops rendering. Harmost releases it at upstream end-of-stream —
# but Pingora paces upstream reads against downstream writes, so once a slow
# client blocks the downstream write, end-of-stream never arrives and the
# permit is held for the client's download instead.
#
# This sweep finds the response size at which that starts to bite. Small
# responses land in socket buffers, the origin finishes independently, and
# capacity comes back promptly. Past that, slow readers occupy render slots.
#
# The real fix is a bounded decoupling buffer, which Pingora's streaming proxy
# model does not offer directly. Until then `timeouts.downstream_write` is the
# bound.
set -uo pipefail
set +m
cd "$(dirname "$0")/.."
cargo build --workspace -q || exit 1
cleanup() { pkill -9 -f 'target/debug/slow-origin' 2>/dev/null; pkill -9 -f 'target/debug/harmost run' 2>/dev/null; }
trap cleanup EXIT

cat > /tmp/slowclient.yaml <<'EOF'
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
cache:
  enabled: false
routes:
  - id: pages
    match: "/**"
    class: public_ssr
EOF

echo "ceiling = 2, two slow readers at 32KB/s, then one normal request"
echo
printf '  %-10s %-8s %-10s %s\n' "body" "status" "latency" "capacity returned?"
for MIB in 1 2 4 8 16; do
  cleanup; sleep 1
  ./target/debug/slow-origin 3000 50 >/dev/null 2>&1 & disown
  sleep 1
  ./target/debug/harmost run --config /tmp/slowclient.yaml >/tmp/h.log 2>&1 & disown
  sleep 2
  curl -s -o /dev/null --limit-rate 32k "http://127.0.0.1:8080/big/1/$MIB" & disown
  curl -s -o /dev/null --limit-rate 32k "http://127.0.0.1:8080/big/2/$MIB" & disown
  sleep 3
  START=$(date +%s%N)
  CODE=$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 "http://127.0.0.1:8080/quick")
  MS=$(( ($(date +%s%N) - START) / 1000000 ))
  if [ "$CODE" = "200" ] && [ "$MS" -lt 1000 ]; then VERDICT="yes"; else VERDICT="NO — permits held"; fi
  printf '  %-10s %-8s %-10s %s\n' "${MIB}MiB" "$CODE" "${MS}ms" "$VERDICT"
done
echo
echo "Known gap: past the buffer threshold, a slow reader occupies a render slot."
