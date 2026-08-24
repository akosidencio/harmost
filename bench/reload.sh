#!/usr/bin/env bash
# Config reload on SIGHUP, including the case that matters: a bad config is
# refused and the running one keeps serving.
set -uo pipefail
set +m
cd "$(dirname "$0")/.."
cargo build --workspace -q || exit 1
cleanup() { pkill -9 -f 'target/debug/slow-origin' 2>/dev/null; pkill -9 -f 'target/debug/harmost run' 2>/dev/null; }
trap cleanup EXIT
cleanup; sleep 1

cat > /tmp/reload.yaml <<'EOF'
version: 1
server:
  listen: "127.0.0.1:8080"
origin:
  upstreams: ["127.0.0.1:3000"]
  concurrency:
    max: 100
cache:
  enabled: false
routes:
  - id: pages
    match: "/**"
    class: public_ssr
    concurrency:
      max: 1
EOF
cp /tmp/reload.yaml /tmp/reload.bak

./target/debug/slow-origin 3000 400 >/dev/null 2>&1 & disown
sleep 1
./target/debug/harmost run --config /tmp/reload.yaml >/tmp/h.log 2>&1 & disown
sleep 2
PID=$(pgrep -f 'target/debug/harmost run' | head -1)

burst() { seq 1 6 | xargs -P 6 -I{} curl -s -o /dev/null -w '%{http_code} ' "http://127.0.0.1:8080/$1/{}"; echo; }

echo "route ceiling 1, six concurrent requests"
printf '  '; burst a

echo
echo "SIGHUP with an invalid config (duplicate route id)"
printf 'version: 1\norigin:\n  upstreams: ["127.0.0.1:3000"]\nroutes:\n  - id: dup\n    match: "/a"\n  - id: dup\n    match: "/b"\n' > /tmp/reload.yaml
kill -HUP "$PID"; sleep 1
echo "  $(grep -o 'reload refused.*' /tmp/h.log | tail -1)"
printf '  '; burst b

echo
echo "SIGHUP raising the ceiling to 50"
sed 's/      max: 1$/      max: 50/' /tmp/reload.bak > /tmp/reload.yaml
kill -HUP "$PID"; sleep 1
echo "  $(grep -o 'config reloaded.*' /tmp/h.log | tail -1)"
printf '  '; burst c
