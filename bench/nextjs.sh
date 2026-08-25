#!/usr/bin/env bash
# A real Next.js App Router integration proof: one Harmost process, three
# standalone origins, and machine-checked public/private/streaming behavior.
set -euo pipefail

cd "$(dirname "$0")/.."

COMPOSE=(docker compose -p harmost-nextjs-fixture -f compose.nextjs.yaml)
PROXY_URL=${PROXY_URL:-http://127.0.0.1:18080}
METRICS_URL=${METRICS_URL:-http://127.0.0.1:19090}
CONCURRENCY=${CONCURRENCY:-24}
RESULT_DIR=$(mktemp -d)

down_stack() {
  "${COMPOSE[@]}" down --remove-orphans >/dev/null 2>&1 || true
}

cleanup() {
  down_stack
  rm -rf "$RESULT_DIR"
}
trap cleanup EXIT

fail() {
  echo "FAIL: $*" >&2
  echo "Harmost logs:" >&2
  "${COMPOSE[@]}" logs --no-color --tail=80 harmost >&2 || true
  exit 1
}

metric_sum() {
  local route=$1
  curl -fsS "$METRICS_URL/metrics" | awk -v route="$route" '
    /^harmost_origin_requests_total[{]/ && index($1, "route=\"" route "\"") {
      sum += $2
    }
    END { print sum + 0 }
  '
}

wait_until_ready() {
  local attempt
  for attempt in $(seq 1 90); do
    if curl -fsS -o /dev/null "$PROXY_URL/healthz" \
      && curl -fsS -o /dev/null "$METRICS_URL/metrics"; then
      return 0
    fi
    sleep 1
  done
  return 1
}

echo "Building and starting one Harmost instance with three Next.js origins..."
down_stack
"${COMPOSE[@]}" up --build --detach
wait_until_ready || fail "services did not become ready"

RUN_ID="$(date +%s)-$$"

echo
echo "1/7 identical public SSR requests coalesce across the origin pool"
BEFORE=$(metric_sum products)
seq 1 "$CONCURRENCY" | xargs -P "$CONCURRENCY" -I{} \
  curl -sS -o "$RESULT_DIR/coalesce-{}.html" -w '%{http_code}\n' \
  "$PROXY_URL/products/coalesce-$RUN_ID" > "$RESULT_DIR/coalesce-status"
SERVED=$(grep -c '^200$' "$RESULT_DIR/coalesce-status" || true)
AFTER=$(metric_sum products)
ORIGIN_REQUESTS=$((AFTER - BEFORE))
[ "$SERVED" -eq "$CONCURRENCY" ] || fail "served $SERVED/$CONCURRENCY coalescing requests"
[ "$ORIGIN_REQUESTS" -eq 1 ] || fail "$CONCURRENCY identical requests reached the origins $ORIGIN_REQUESTS times"
RENDER_IDS=$(sed -n 's/.*data-render-id="\([^"]*\)".*/\1/p' "$RESULT_DIR"/coalesce-*.html | sort -u | wc -l | tr -d ' ')
[ "$RENDER_IDS" -eq 1 ] || fail "coalesced clients received $RENDER_IDS render ids"
echo "PASS: $CONCURRENCY responses, one origin render, one shared render id"

echo
echo "2/7 distinct paths are distributed across all three Next.js origins"
for index in $(seq 1 18); do
  curl -fsS "$PROXY_URL/products/spread-$RUN_ID-$index" > "$RESULT_DIR/spread-$index.html"
done
INSTANCES=$(sed -n 's/.*data-origin-instance="\([^"]*\)".*/\1/p' "$RESULT_DIR"/spread-*.html | sort -u)
INSTANCE_COUNT=$(echo "$INSTANCES" | sed '/^$/d' | wc -l | tr -d ' ')
[ "$INSTANCE_COUNT" -eq 3 ] || fail "expected three origins, observed $INSTANCE_COUNT: $INSTANCES"
echo "PASS: observed $(echo "$INSTANCES" | tr '\n' ' ')"

echo
echo "3/7 HTML and React Server Component payloads use separate cache keys"
RSC_PATH="/products/rsc-$RUN_ID"
RSC_URL="$PROXY_URL$RSC_PATH"
# This is the router tree emitted by the fixture homepage. Next canonicalizes
# the placeholder `_rsc` value once, just as its browser client does, before it
# returns the actual component payload.
RSC_TREE='%5B%22%22%2C%7B%22children%22%3A%5B%22__PAGE__%22%2C%7B%7D%2Cnull%2Cnull%2C4096%5D%7D%2Cnull%2Cnull%2C4112%5D'
BEFORE=$(metric_sum products)
curl -fsS -D "$RESULT_DIR/html.headers" -o "$RESULT_DIR/html.body" "$RSC_URL"
curl -sS -D "$RESULT_DIR/rsc-redirect.headers" -o /dev/null \
  -H 'RSC: 1' -H 'Next-Url: /' -H "Next-Router-State-Tree: $RSC_TREE" \
  "$RSC_URL?_rsc=fixture"
RSC_LOCATION=$(sed -n 's/^location: //Ip' "$RESULT_DIR/rsc-redirect.headers" | tr -d '\r' | tail -1)
case "$RSC_LOCATION" in
  "$RSC_PATH"'?_rsc='*) ;;
  *) fail "Next.js did not return a canonical RSC location: ${RSC_LOCATION:-missing}" ;;
esac
curl -fsS -D "$RESULT_DIR/rsc.headers" -o "$RESULT_DIR/rsc.body" \
  -H 'RSC: 1' -H 'Next-Url: /' -H "Next-Router-State-Tree: $RSC_TREE" \
  "$PROXY_URL$RSC_LOCATION"
# Both variants should now be cache hits and must remain different types.
curl -fsS -o "$RESULT_DIR/rsc-again.body" \
  -H 'RSC: 1' -H 'Next-Url: /' -H "Next-Router-State-Tree: $RSC_TREE" \
  "$PROXY_URL$RSC_LOCATION"
curl -fsS -o "$RESULT_DIR/html-again.body" "$RSC_URL"
AFTER=$(metric_sum products)
ORIGIN_REQUESTS=$((AFTER - BEFORE))
[ "$ORIGIN_REQUESTS" -eq 3 ] || fail "HTML plus canonicalized RSC sequence used $ORIGIN_REQUESTS origin requests instead of 3"
grep -qi '^content-type: text/html' "$RESULT_DIR/html.headers" || fail "document request was not HTML"
grep -qi '^content-type: text/x-component' "$RESULT_DIR/rsc.headers" || fail "RSC request was not a component payload"
cmp -s "$RESULT_DIR/html.body" "$RESULT_DIR/html-again.body" || fail "cached HTML response changed after an RSC request"
cmp -s "$RESULT_DIR/rsc.body" "$RESULT_DIR/rsc-again.body" || fail "cached RSC response changed on reuse"
echo "PASS: HTML and canonical RSC variants cached independently without mixing"

echo
echo "4/7 Set-Cookie responses are never shared"
PRIVATE_COUNT=16
BEFORE=$(metric_sum private-session)
seq 1 "$PRIVATE_COUNT" | xargs -P "$PRIVATE_COUNT" -I{} \
  curl -sS -o "$RESULT_DIR/private-{}.json" -w '%{http_code}\n' \
  "$PROXY_URL/api/private-session" > "$RESULT_DIR/private-status"
SERVED=$(grep -c '^200$' "$RESULT_DIR/private-status" || true)
AFTER=$(metric_sum private-session)
ORIGIN_REQUESTS=$((AFTER - BEFORE))
SESSIONS=$(sed -n 's/.*"session_id":"\([^"]*\)".*/\1/p' "$RESULT_DIR"/private-*.json | sort -u | wc -l | tr -d ' ')
[ "$SERVED" -eq "$PRIVATE_COUNT" ] || fail "served $SERVED/$PRIVATE_COUNT private requests"
[ "$ORIGIN_REQUESTS" -eq "$PRIVATE_COUNT" ] || fail "$PRIVATE_COUNT private requests caused $ORIGIN_REQUESTS origin requests"
[ "$SESSIONS" -eq "$PRIVATE_COUNT" ] || fail "$PRIVATE_COUNT private requests returned $SESSIONS sessions"
echo "PASS: $PRIVATE_COUNT requests, $PRIVATE_COUNT renders, $PRIVATE_COUNT sessions"

echo
echo "5/7 Next-Action mutation requests bypass a deliberately public route"
MUTATION_COUNT=12
BEFORE=$(metric_sum action-probe)
seq 1 "$MUTATION_COUNT" | xargs -P "$MUTATION_COUNT" -I{} \
  curl -sS -X POST -H 'Next-Action: fixture-probe' \
  -o "$RESULT_DIR/mutation-{}.json" -w '%{http_code}\n' \
  "$PROXY_URL/api/action-probe" > "$RESULT_DIR/mutation-status"
SERVED=$(grep -c '^200$' "$RESULT_DIR/mutation-status" || true)
AFTER=$(metric_sum action-probe)
ORIGIN_REQUESTS=$((AFTER - BEFORE))
MUTATIONS=$(sed -n 's/.*"mutation_id":"\([^"]*\)".*/\1/p' "$RESULT_DIR"/mutation-*.json | sort -u | wc -l | tr -d ' ')
[ "$SERVED" -eq "$MUTATION_COUNT" ] || fail "served $SERVED/$MUTATION_COUNT mutation requests"
[ "$ORIGIN_REQUESTS" -eq "$MUTATION_COUNT" ] || fail "$MUTATION_COUNT mutations caused $ORIGIN_REQUESTS origin requests"
[ "$MUTATIONS" -eq "$MUTATION_COUNT" ] || fail "$MUTATION_COUNT mutations returned $MUTATIONS mutation ids"
echo "PASS: $MUTATION_COUNT mutations bypassed cache reuse and coalescing"

echo
echo "6/7 Draft Mode bypasses a cached public preview without contaminating it"
BEFORE=$(metric_sum preview)
curl -fsS -o "$RESULT_DIR/preview-public.html" "$PROXY_URL/preview"
curl -sS -D "$RESULT_DIR/draft.headers" -o /dev/null "$PROXY_URL/api/draft"
DRAFT_COOKIE=$(sed -n 's/^[Ss]et-[Cc]ookie: \([^;]*\).*/\1/p' "$RESULT_DIR/draft.headers" | tr -d '\r' | paste -sd ';' -)
[ -n "$DRAFT_COOKIE" ] || fail "Draft Mode endpoint returned no cookie"
curl -fsS -H "Cookie: $DRAFT_COOKIE" -o "$RESULT_DIR/preview-draft.html" "$PROXY_URL/preview"
curl -fsS -o "$RESULT_DIR/preview-public-again.html" "$PROXY_URL/preview"
AFTER=$(metric_sum preview)
ORIGIN_REQUESTS=$((AFTER - BEFORE))
[ "$ORIGIN_REQUESTS" -eq 2 ] || fail "public/draft/public preview sequence used $ORIGIN_REQUESTS origin requests instead of 2"
grep -q 'Unpublished winter catalog' "$RESULT_DIR/preview-draft.html" || fail "Draft Mode content was not returned"
cmp -s "$RESULT_DIR/preview-public.html" "$RESULT_DIR/preview-public-again.html" || fail "Draft Mode contaminated the cached public preview"
echo "PASS: draft content bypassed; the public cache entry remained unchanged"

echo
echo "7/7 coalescing preserves a real Suspense stream"
STREAM_COUNT=10
STREAM_URL="$PROXY_URL/flash-sale?run=$RUN_ID"
BEFORE=$(metric_sum flash-sale)
seq 1 "$STREAM_COUNT" | xargs -P "$STREAM_COUNT" -I{} \
  curl -sS -o /dev/null --max-time 20 \
  -w '%{time_starttransfer} %{time_total} %{http_code}\n' \
  "$STREAM_URL" > "$RESULT_DIR/stream-times"
AFTER=$(metric_sum flash-sale)
ORIGIN_REQUESTS=$((AFTER - BEFORE))
SERVED=$(grep -c ' 200$' "$RESULT_DIR/stream-times" || true)
MAX_TTFB=$(awk '{print $1}' "$RESULT_DIR/stream-times" | sort -n | tail -1)
MEDIAN_TOTAL=$(awk '{print $2}' "$RESULT_DIR/stream-times" | sort -n | awk '{v[NR]=$1} END {print v[int(NR/2)+1]}')
STREAMED=$(awk -v first="$MAX_TTFB" -v total="$MEDIAN_TOTAL" 'BEGIN { print (first < total / 2) ? 1 : 0 }')
[ "$SERVED" -eq "$STREAM_COUNT" ] || fail "served $SERVED/$STREAM_COUNT streaming requests"
[ "$ORIGIN_REQUESTS" -eq 1 ] || fail "$STREAM_COUNT streaming requests caused $ORIGIN_REQUESTS origin requests"
[ "$STREAMED" -eq 1 ] || fail "max TTFB ${MAX_TTFB}s was not below half of median total ${MEDIAN_TOTAL}s"
echo "PASS: one origin render; max TTFB ${MAX_TTFB}s, median total ${MEDIAN_TOTAL}s"

echo
echo "PASS: real Next.js integration proof completed"
