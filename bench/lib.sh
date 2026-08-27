# Shared harness for the bench/ scripts. Source it, never execute it.
#
# The scripts in this directory are the evidence behind the claims in the
# README, so the harness they share has to be trustworthy before the numbers
# are. Three habits it exists to remove:
#
#   * `pkill -f target/debug/harmost` kills every Harmost on the machine,
#     including another developer's, another benchmark's, and — on a laptop
#     running the fixture — the one under test in a different terminal. Every
#     process here is started through `bench_spawn`, which records the exact
#     pid, and torn down by that pid alone.
#   * Fixed ports 3000/8080/9090 collide with whatever is already running and
#     turn a benchmark into a measurement of somebody else's server. Ports are
#     allocated from the kernel's free range per run.
#   * `sleep 2 && hope` reports a startup race as a benchmark failure. Readiness
#     is polled.
#
# Every script also records the parameters it ran with next to its results, so
# a number can be compared against another machine instead of being taken as a
# universal baseline.

# Bash 3.2 (the macOS system bash) is the floor: no associative arrays, no
# ${var,,}, no `mapfile`.

set -uo pipefail

BENCH_NAME=""
BENCH_DIR=""
BENCH_PIDS=""
BENCH_STATUS="incomplete"
BENCH_FAIL_REASON=""
BENCH_PARAM_KEYS=""
BENCH_RESULT_KEYS=""

bench_cpu_count() {
  { command -v nproc >/dev/null && nproc; } \
    || sysctl -n hw.ncpu 2>/dev/null \
    || echo unknown
}

# ---------------------------------------------------------------- lifecycle

bench_init() {
  BENCH_NAME=$1
  BENCH_DIR=$(mktemp -d "${TMPDIR:-/tmp}/harmost-bench-XXXXXX")
  mkdir -p "$BENCH_DIR/pids" "$BENCH_DIR/logs"
  BENCH_STARTED_AT=$(date -u +%Y-%m-%dT%H:%M:%SZ)
  trap bench_cleanup EXIT
  trap 'bench_fail "interrupted"' INT TERM

  bench_param os "$(uname -s)"
  bench_param arch "$(uname -m)"
  bench_param cpus "$(bench_cpu_count)"
  bench_param rustc "$(rustc --version 2>/dev/null | awk '{print $2}')"
  bench_param profile "${BENCH_PROFILE:-debug}"
  bench_param commit "$(git -C "$BENCH_ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
}

bench_cleanup() {
  local code=$?
  bench_stop_all
  bench_report_write
  if [ -n "${BENCH_KEEP_DIR:-}" ]; then
    echo "  (working directory kept at $BENCH_DIR)"
  else
    rm -rf "$BENCH_DIR"
  fi
  return $code
}

# Build the workspace once, from the repository root, before anything is
# spawned. A benchmark that races the compiler measures the compiler.
# `BENCH_FEATURES` is for the scripts that need an optional feature compiled in
# — `tls` today. It is a parameter rather than a default because the TLS stack
# is a two-minute compile that every other benchmark would pay for.
bench_build() {
  local flags=""
  if [ "${BENCH_PROFILE:-debug}" = "release" ]; then flags="--release"; fi
  if [ -n "${BENCH_FEATURES:-}" ]; then flags="$flags --features $BENCH_FEATURES"; fi
  cargo build --workspace $flags -q || bench_fail "workspace build failed"
}

bench_bin() {
  echo "$BENCH_ROOT/target/${BENCH_PROFILE:-debug}/$1"
}

# ------------------------------------------------------------------- ports

# Is something listening on this port right now?
bench_port_open() {
  (exec 3<>"/dev/tcp/$1/$2") >/dev/null 2>&1
}

# Hand out a port nothing is listening on, and never the same one twice within
# one run. Racy in principle against the rest of the machine; in practice a
# random draw from 20000 unused ports beats a hardcoded 8080 by a wide margin.
#
# The ledger of already-issued ports lives in a file, not a variable: callers
# use this as `PORT=$(bench_free_port)`, and a command substitution runs in a
# subshell whose variable assignments are discarded — so a variable would hand
# out the same port twice.
bench_free_port() {
  local port attempt ledger="$BENCH_DIR/ports"
  touch "$ledger"
  for attempt in $(seq 1 200); do
    port=$(( 20000 + RANDOM % 20000 ))
    grep -qx "$port" "$ledger" && continue
    if ! bench_port_open 127.0.0.1 "$port"; then
      echo "$port" >> "$ledger"
      printf '%s\n' "$port"
      return 0
    fi
  done
  bench_fail "no free TCP port found after 200 attempts"
}

# --------------------------------------------------------------- processes

# Start a process, remember its exact pid, and send its output to a log this
# run owns. Nothing else in the harness may start a long-lived process.
bench_spawn() {
  local name=$1; shift
  "$@" >"$BENCH_DIR/logs/$name.log" 2>&1 &
  local pid=$!
  disown 2>/dev/null || true
  echo "$pid" > "$BENCH_DIR/pids/$name"
  BENCH_PIDS="$BENCH_PIDS $pid"
}

bench_pid() {
  cat "$BENCH_DIR/pids/$1" 2>/dev/null
}

bench_log() {
  echo "$BENCH_DIR/logs/$1.log"
}

bench_alive() {
  kill -0 "$1" 2>/dev/null
}

# Stop one named process politely, then insist.
bench_stop() {
  local pid attempt
  pid=$(bench_pid "$1")
  [ -n "$pid" ] || return 0
  kill -TERM "$pid" 2>/dev/null || true
  for attempt in $(seq 1 50); do
    bench_alive "$pid" || { rm -f "$BENCH_DIR/pids/$1"; return 0; }
    sleep 0.1
  done
  kill -KILL "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
  rm -f "$BENCH_DIR/pids/$1"
}

bench_stop_all() {
  local file
  [ -d "${BENCH_DIR:-/nonexistent}/pids" ] || return 0
  for file in "$BENCH_DIR"/pids/*; do
    [ -e "$file" ] || continue
    bench_stop "$(basename "$file")"
  done
}

# --------------------------------------------------------------- readiness

bench_wait_port() {
  local host=$1 port=$2 label=${3:-"$1:$2"} attempt
  for attempt in $(seq 1 300); do
    bench_port_open "$host" "$port" && return 0
    sleep 0.1
  done
  bench_fail "$label never accepted a connection on $host:$port"
}

bench_wait_http() {
  local url=$1 label=${2:-$1} attempt
  for attempt in $(seq 1 300); do
    if curl -fsS -o /dev/null --max-time 2 "$url" 2>/dev/null; then return 0; fi
    sleep 0.1
  done
  bench_fail "$label never answered $url"
}

# Wait for a line to appear in a spawned process's log. Used where the
# observable event is a log line rather than a socket — a config reload, for
# example, has no port of its own to poll.
bench_wait_log() {
  local name=$1 pattern=$2 attempt
  for attempt in $(seq 1 100); do
    grep -q "$pattern" "$(bench_log "$name")" 2>/dev/null && return 0
    sleep 0.1
  done
  return 1
}

# ----------------------------------------------------------------- origins

# Start bench/slow-origin and wait until it serves. Returns the port on stdout
# via the variable the caller names, because bash 3.2 has no nameref.
bench_start_origin() { # name, render_ms -> echoes port
  local name=$1 render_ms=$2 port
  port=$(bench_free_port)
  bench_spawn "$name" "$(bench_bin slow-origin)" "$port" "$render_ms"
  bench_wait_port 127.0.0.1 "$port" "slow-origin"
  echo "$port"
}

# The origin is the witness. Ask it directly rather than parsing headers off a
# response the proxy may have served from its own cache, or counting lines in
# the proxy's log — both of which measure the component under test.
bench_origin_stat() { # port, field
  curl -fsS --max-time 5 "http://127.0.0.1:$1/__stats" 2>/dev/null \
    | sed -n "s/.*\"$2\":\([0-9]*\).*/\1/p"
}

bench_origin_reset() {
  curl -fsS -o /dev/null --max-time 5 "http://127.0.0.1:$1/__reset" 2>/dev/null \
    || bench_fail "slow-origin on port $1 did not answer /__reset"
}

# ------------------------------------------------------------------ config

# Render a bench config template, substituting the ports this run allocated.
# The templates keep their placeholders in the repository so that no benchmark
# can quietly depend on a port that happened to be free on one laptop.
bench_render_config() { # template, out, then KEY=VALUE...
  local template=$1 out=$2; shift 2
  local script="" pair
  for pair in "$@"; do
    script="$script s|@${pair%%=*}@|${pair#*=}|g;"
  done
  sed "$script" "$template" > "$out" || bench_fail "could not render $template"
  if grep -q '@[A-Z_]*@' "$out"; then
    bench_fail "unsubstituted placeholder left in $out: $(grep -o '@[A-Z_]*@' "$out" | sort -u | tr '\n' ' ')"
  fi
}

bench_start_harmost() { # name, config, listen_port [, metrics_port]
  local name=$1 config=$2 listen=$3 metrics=${4:-}
  bench_spawn "$name" "$(bench_bin harmost)" run --config "$config"
  bench_wait_port 127.0.0.1 "$listen" "harmost"
  [ -n "$metrics" ] && bench_wait_port 127.0.0.1 "$metrics" "harmost metrics"
  return 0
}

# ------------------------------------------------------- parameters/results

bench_param() {
  BENCH_PARAM_KEYS="$BENCH_PARAM_KEYS $1"
  eval "BENCH_PARAM_$1=\$2"
}

bench_result() {
  BENCH_RESULT_KEYS="$BENCH_RESULT_KEYS $1"
  eval "BENCH_RESULT_$1=\$2"
}

bench_get() { eval "printf '%s' \"\${$1-}\""; }

# The parameter block is printed with the result, not buried in the script, so
# a pasted benchmark output says what produced it.
bench_print_params() {
  local key first=1
  printf 'parameters: '
  for key in $BENCH_PARAM_KEYS; do
    [ $first = 1 ] || printf ', '
    printf '%s=%s' "$key" "$(bench_get "BENCH_PARAM_$key")"
    first=0
  done
  printf '\n'
}

# One JSON object per run. `BENCH_REPORT_DIR` is set by CI so results can be
# published as an artifact alongside the parameters that produced them.
bench_report_write() {
  [ -n "${BENCH_REPORT_DIR:-}" ] || return 0
  [ -n "$BENCH_NAME" ] || return 0
  mkdir -p "$BENCH_REPORT_DIR"
  {
    printf '{"benchmark":"%s","started_at":"%s","status":"%s"' \
      "$BENCH_NAME" "${BENCH_STARTED_AT:-}" "$BENCH_STATUS"
    [ -n "$BENCH_FAIL_REASON" ] && printf ',"reason":"%s"' "$BENCH_FAIL_REASON"
    printf ',"parameters":{'
    local key first=1
    for key in $BENCH_PARAM_KEYS; do
      [ $first = 1 ] || printf ','
      printf '"%s":"%s"' "$key" "$(bench_get "BENCH_PARAM_$key")"
      first=0
    done
    printf '},"results":{'
    first=1
    for key in $BENCH_RESULT_KEYS; do
      [ $first = 1 ] || printf ','
      printf '"%s":"%s"' "$key" "$(bench_get "BENCH_RESULT_$key")"
      first=0
    done
    printf '}}\n'
  } > "$BENCH_REPORT_DIR/$BENCH_NAME.json"
}

# --------------------------------------------------------------- assertions

# Every check is machine-evaluated and every failure names both sides. A
# benchmark that prints numbers for a human to eyeball is not evidence.

bench_fail() {
  BENCH_STATUS="fail"
  BENCH_FAIL_REASON="$*"
  echo "FAIL: $*" >&2
  exit 1
}

bench_pass() {
  BENCH_STATUS="pass"
  echo "PASS: $*"
}

bench_is_int() { case "${1:-}" in ''|*[!0-9]*) return 1 ;; *) return 0 ;; esac; }

bench_assert_int() { # value, label
  bench_is_int "${1:-}" || bench_fail "$2 was not a number: '${1:-<empty>}'"
}

bench_assert_eq() { # actual, expected, label
  bench_assert_int "$1" "$3"
  [ "$1" -eq "$2" ] || bench_fail "$3: expected $2, observed $1"
}

bench_assert_le() { # actual, bound, label
  bench_assert_int "$1" "$3"
  [ "$1" -le "$2" ] || bench_fail "$3: expected at most $2, observed $1"
}

bench_assert_gt() { # actual, bound, label
  bench_assert_int "$1" "$3"
  [ "$1" -gt "$2" ] || bench_fail "$3: expected more than $2, observed $1"
}

# Compare floating-point seconds without depending on bc.
bench_lt_float() { awk -v a="$1" -v b="$2" 'BEGIN { exit !(a < b) }'; }

bench_median() { sort -n | awk '{ v[NR]=$1 } END { print (NR ? v[int(NR/2)+1] : "") }'; }
bench_max() { sort -n | tail -1; }
