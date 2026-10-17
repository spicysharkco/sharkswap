# shellcheck shell=bash
# shellcheck disable=SC2034  # variables here are read by the sourcing test/cli.sh
# Shared helpers for test/cli.sh. Sourced, not run.
#
# Every command under test runs through `cs`, which starts the binary under
# `env -i` with a HOME (and CLAUDE_CONFIG_DIR-free XDG_* dirs) inside the
# sandbox root $T. Nothing here ever points at the real home directory.
#
# The binary under test is $CS_BIN (default: the test-hooks build that test/run.sh
# makes, target/test-hooks/release/sharkswap in this checkout). It must be
# built with `--features test-hooks`: that is what lets the suite point the
# binary at a stub of the macOS `security` tool, and force the macOS code path on
# Linux, so no test can reach a real Keychain. A build without the feature has
# no such hooks, and the suite refuses to run with it.

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CS_BIN="${CS_BIN:-$ROOT/target/test-hooks/release/sharkswap}"
[[ -x "$CS_BIN" ]] || { echo "helpers: $CS_BIN is not an executable; build it (cargo build --release --features test-hooks --target-dir target/test-hooks) or set CS_BIN" >&2; exit 2; }
LC_ALL=C grep -aq SHARKSWAP_TEST_SECURITY "$CS_BIN" || {
  echo "helpers: $CS_BIN was built without the test hooks (cargo build --features test-hooks); on a Mac it would use the real Keychain" >&2
  exit 2
}

: "${T:?T (the sandbox root) must be set by the caller}"
case "$T" in
  "$ROOT"/.test-tmp/*) ;;
  *) echo "helpers: refusing to run with sandbox root '$T' outside $ROOT/.test-tmp" >&2; exit 2 ;;
esac

PASS=0
FAIL=0
FAILED_NAMES=()
CURRENT=""

# Result of the last `cs` call.
OUT=""
ERR=""
RC=0

section() { CURRENT="$1"; printf '\n== %s\n' "$1"; }

pass() { PASS=$((PASS + 1)); printf '  ok   %s\n' "$1"; }
fail() {
  FAIL=$((FAIL + 1)); FAILED_NAMES+=("$CURRENT: $1")
  printf '  FAIL %s\n' "$1"
  [[ -z "${2:-}" ]] || printf '       %s\n' "$2"
}

assert_eq() {  # name, expected, actual
  if [[ "$2" == "$3" ]]; then pass "$1"; else fail "$1" "expected [$2] got [$3]"; fi
}
assert_rc() {  # name, expected status
  if [[ "$RC" == "$2" ]]; then pass "$1"; else fail "$1" "expected exit $2 got $RC; stderr: ${ERR:0:300}"; fi
}
assert_out_has() {  # name, fixed string expected in stdout
  if [[ "$OUT" == *"$2"* ]]; then pass "$1"; else fail "$1" "stdout lacks [$2]; stdout: ${OUT:0:300}"; fi
}
assert_err_has() {  # name, fixed string expected in stderr
  if [[ "$ERR" == *"$2"* ]]; then pass "$1"; else fail "$1" "stderr lacks [$2]; stderr: ${ERR:0:300}"; fi
}
assert_out_lacks() {  # name, fixed string that must not be in stdout or stderr
  if [[ "$OUT$ERR" != *"$2"* ]]; then pass "$1"; else fail "$1" "output contains [$2]"; fi
}
assert_file() {  # name, path
  if [[ -e "$2" ]]; then pass "$1"; else fail "$1" "missing $2"; fi
}
assert_no_file() {  # name, path
  if [[ ! -e "$2" ]]; then pass "$1"; else fail "$1" "unexpected $2"; fi
}
assert_mode() {  # name, path, octal mode
  local m; m="$(stat -c '%a' "$2" 2>/dev/null || stat -f '%Lp' "$2" 2>/dev/null || echo "?")"
  assert_eq "$1" "$3" "$m"
}

# --- sandbox ----------------------------------------------------------------

STUB_PID=""
STUB_PORT=""

# new_world NAME: a clean HOME for one scenario, with a fake `claude` in PATH and
# a fake `security` that the binary is told to use (SHARKSWAP_TEST_SECURITY,
# set in cs_exec): the binary calls the real one by absolute path otherwise.
new_world() {
  W="$T/$1"
  rm -rf "${W:?}"
  H="$W/home"; mkdir -p "$H/.claude" "$W/tmp" "$W/config" "$W/state" "$W/cache" "$W/bin"
  CLAUDE_LOG="$W/claude-calls.log"; : > "$CLAUDE_LOG"
  cat > "$W/bin/claude" <<'EOF'
#!/bin/sh
# Stand-in for the claude CLI: logs its arguments, answers --version.
printf '%s\n' "$*" >> "${CLAUDE_STUB_LOG:-/dev/null}"
case "$1" in --version) echo "2.1.0 (Claude Code)" ;; *) echo OK ;; esac
EOF
  chmod +x "$W/bin/claude"
  # Stand-in for macOS `security`. Each call's arguments are one line of
  # $SECURITY_STUB_LOG, and what `security -i` is fed goes to $SECURITY_STUB_STDIN
  # (made-up tokens only, in the sandbox). `find-generic-password` hands back the
  # file $SECURITY_STUB_ITEM, or finds nothing (exit 44). It never prints what it
  # is fed. A test can set, through CS_ENV:
  #   SECURITY_STUB_FAIL=TEXT      a write prints TEXT on stderr and exits 36 ...
  #   SECURITY_STUB_OK_WRITES=N    ... except the first N writes (needs _COUNT)
  #   SECURITY_STUB_COUNT=FILE     where the stub counts its writes
  #   SECURITY_STUB_SABOTAGE=PATH  the first write replaces PATH by a directory,
  #                                so a later write to PATH fails (needs _COUNT)
  SECURITY_LOG="$W/security-calls.log"; : > "$SECURITY_LOG"
  cat > "$W/bin/security" <<'EOF'
#!/bin/sh
printf '%s\n' "$*" >> "${SECURITY_STUB_LOG:-/dev/null}"
case "$1" in
  -i | add-generic-password)
    [ "$1" != -i ] || cat >> "${SECURITY_STUB_STDIN:-/dev/null}"
    n=1
    if [ -n "$SECURITY_STUB_COUNT" ]; then
      n=0
      [ ! -f "$SECURITY_STUB_COUNT" ] || n=$(cat "$SECURITY_STUB_COUNT")
      n=$((n + 1))
      echo "$n" > "$SECURITY_STUB_COUNT"
    fi
    if [ -n "$SECURITY_STUB_FAIL" ] && [ "$n" -gt "${SECURITY_STUB_OK_WRITES:-0}" ]; then
      echo "$SECURITY_STUB_FAIL" >&2
      exit 36
    fi
    if [ -n "$SECURITY_STUB_SABOTAGE" ] && [ "$n" -eq 1 ]; then
      rm -f "$SECURITY_STUB_SABOTAGE"
      mkdir "$SECURITY_STUB_SABOTAGE"
    fi
    ;;
  find-generic-password)
    if [ -n "$SECURITY_STUB_ITEM" ]; then cat "$SECURITY_STUB_ITEM"; else exit 44; fi
    ;;
esac
EOF
  chmod +x "$W/bin/security"
  API_BASE_FOR_TESTS="http://127.0.0.1:9"   # closed port: any stray probe fails fast
}

# cs_exec ARGS...: the binary under `env -i`, with the sandbox as its whole
# world. Extra environment goes in CS_ENV (an array of NAME=value words).
CS_ENV=()
CS_WRAP=()   # a command to put in front, e.g. strace; used by the containment scenario
cs_exec() {
  ${CS_WRAP[@]+"${CS_WRAP[@]}"} env -i \
    PATH="$W/bin:$PATH" HOME="$H" USER=tester LANG=C.UTF-8 TERM=dumb \
    TMPDIR="$W/tmp" XDG_CONFIG_HOME="$W/config" XDG_STATE_HOME="$W/state" XDG_CACHE_HOME="$W/cache" \
    CLAUDE_STUB_LOG="$CLAUDE_LOG" SECURITY_STUB_LOG="$SECURITY_LOG" \
    SHARKSWAP_TEST_HOOKS=1 SHARKSWAP_TEST_SECURITY="$W/bin/security" \
    SHARKSWAP_API_BASE="${API_BASE_OVERRIDE:-$API_BASE_FOR_TESTS}" \
    QUICK="${QUICK:-1}" ASSUME_YES="${ASSUME_YES:-1}" \
    ${CS_ENV[@]+"${CS_ENV[@]}"} \
    "${CS_PROG:-$CS_BIN}" "$@"
}

# cs ARGS...: run it to completion. Sets OUT, ERR, RC. Standard input is
# CS_STDIN (a file) when set, otherwise empty.
cs() {
  set +e
  cs_exec "$@" >"$W/out.txt" 2>"$W/err.txt" <"${CS_STDIN:-/dev/null}"
  RC=$?
  set -e
  OUT="$(cat "$W/out.txt")"; ERR="$(cat "$W/err.txt")"
}

# cs_limit ARGS...: like cs, but SIGALRM ends the binary after 20 seconds (RC 142),
# so a read that blocks on a FIFO fails a check instead of hanging the suite. perl
# is used because timeout(1) is not on every system the suite runs on.
cs_limit() {
  local wrap=(${CS_WRAP[@]+"${CS_WRAP[@]}"})
  CS_WRAP=(perl -e 'alarm 20; exec @ARGV')
  cs "$@"
  CS_WRAP=(${wrap[@]+"${wrap[@]}"})
}

# cs_tty ARGS...: like cs, but with standard input, output and error on a
# terminal (a pseudo-terminal made by script(1)). Sets OUT to what the terminal
# showed and RC to the exit status. Returns 1, setting nothing, when no terminal
# can be had (NO_TTY=1, as in the strace run, which would flag the pty devices;
# no script(1); a sandbox without ptys), so the caller can skip.
cs_tty() {
  [[ "${NO_TTY:-}" != 1 ]] && command -v script >/dev/null 2>&1 || return 1
  local runner="$W/tty-run.sh" w line
  {
    printf '#!/bin/sh\nenv -i'
    for w in PATH="$W/bin:$PATH" HOME="$H" USER=tester LANG=C.UTF-8 TERM=dumb TMPDIR="$W/tmp" \
             XDG_CONFIG_HOME="$W/config" XDG_STATE_HOME="$W/state" XDG_CACHE_HOME="$W/cache" \
             SHARKSWAP_API_BASE="$API_BASE_FOR_TESTS" ASSUME_YES=1 \
             SHARKSWAP_TEST_HOOKS=1 SHARKSWAP_TEST_SECURITY="$W/bin/security"; do
      printf ' %q' "$w"
    done
    printf ' %q "$@"\necho "rc=$?"\n' "$CS_BIN"
  } > "$runner"
  chmod +x "$runner"
  set +e
  if [[ "$(uname -s)" == Darwin ]]; then
    timeout_run script -q /dev/null "$runner" "$@" >"$W/tty-out.txt" 2>&1 </dev/null
  else
    timeout_run script -qec "$(printf '%q ' "$runner" "$@")" /dev/null >"$W/tty-out.txt" 2>&1 </dev/null
  fi
  set -e
  OUT="$(tr -d '\r' < "$W/tty-out.txt")"; ERR=""
  line="$(grep -E '^rc=[0-9]+$' <<<"$OUT" | tail -n 1 || true)"
  [[ -n "$line" ]] || return 1
  RC="${line#rc=}"
}

# timeout_run CMD...: CMD, killed after 30 seconds where timeout(1) exists.
timeout_run() {
  if command -v timeout >/dev/null 2>&1; then timeout 30 "$@"; else "$@"; fi
}

# cs_bg ARGS...: start it in the background; BG_PID is the pid of the subshell.
BG_PID=""
cs_bg() {
  cs_exec "$@" >"$W/bg-out.txt" 2>"$W/bg-err.txt" </dev/null &
  BG_PID=$!
}

# wait_for_text NAME FILE TEXT: wait up to 10 s for FILE to hold the fixed string
# TEXT, a line the code under test writes. A timeout fails NAME with a message and
# goes on, so a scenario that never gets there reports it instead of passing quietly.
wait_for_text() {
  local n
  for n in $(seq 1 200); do
    if grep -qF -- "$3" "$2" 2>/dev/null; then pass "$1"; return 0; fi
    sleep 0.05
  done
  fail "$1" "timed out after 10 s waiting for [$3] in $2"
}

# fake_token N: a made-up OAuth access token, only ever valid against the stub.
fake_token() { printf 'fake-access-%s' "$1"; }

# mk_login EMAIL N [EXPIRES_MS]: make EMAIL the live login (token N).
mk_login() {
  local email="$1" n="$2" exp="${3:-}"
  [[ -n "$exp" ]] || exp=$(( ($(date +%s) + 3600) * 1000 ))
  mkdir -p "$H/.claude"
  jq -n --arg a "$(fake_token "$n")" --arg r "fake-refresh-$n" --argjson e "$exp" \
    '{claudeAiOauth:{accessToken:$a, refreshToken:$r, expiresAt:$e, subscriptionType:"max"}}' \
    > "$H/.claude/.credentials.json"
  jq -n --arg m "$email" --arg u "00000000-0000-4000-8000-00000000000$n" \
    '{oauthAccount:{emailAddress:$m, accountUuid:$u, organizationName:"Example Org"}}' \
    > "$H/.claude.json"
  cp "$H/.claude.json" "$H/.claude/.claude.json"
}

# save_as ALIAS EMAIL N: log in as EMAIL and snapshot it as ALIAS.
save_as() {
  mk_login "$2" "$3"
  cs save "$1"
  [[ "$RC" == 0 ]] || { echo "setup: save $1 failed: $ERR" >&2; return 1; }
}

live_email() { jq -r '.oauthAccount.emailAddress' "$H/.claude.json"; }
live_token() { jq -r '.claudeAiOauth.accessToken' "$H/.claude/.credentials.json"; }

# --- stub API ---------------------------------------------------------------

STUB_ROUTES=""
STUB_LOG=""

# stub_start: start the loopback API stand-in. Routes go in $STUB_ROUTES.
stub_start() {
  STUB_ROUTES="$W/routes.json"; STUB_LOG="$W/stub.log"
  echo '{}' > "$STUB_ROUTES"; : > "$STUB_LOG"; rm -f "$W/port"
  node "$ROOT/test/stub-api.mjs" --routes "$STUB_ROUTES" --log "$STUB_LOG" --port-file "$W/port" &
  STUB_PID=$!
  local n
  for n in $(seq 1 100); do
    [[ -s "$W/port" ]] && break
    kill -0 "$STUB_PID" 2>/dev/null || { echo "setup: stub died" >&2; return 1; }
    sleep 0.1
  done
  [[ -s "$W/port" ]] || { echo "setup: stub never listened" >&2; return 1; }
  STUB_PORT="$(cat "$W/port")"
  API_BASE_OVERRIDE="http://127.0.0.1:$STUB_PORT"
}

stub_stop() {
  if [[ -n "$STUB_PID" ]]; then
    kill "$STUB_PID" 2>/dev/null || true
    wait "$STUB_PID" 2>/dev/null || true
    STUB_PID=""
  fi
  API_BASE_OVERRIDE=""
}

# stub_route TOKEN_N NAME STATUS [HEADERS_JSON [DELAY_MS]]: answer token N with STATUS.
stub_route() {
  local n="$1" name="$2" status="$3" headers="${4:-}" delay="${5:-0}"
  [[ -n "$headers" ]] || headers='{}'
  stub_route_raw "$(fake_token "$n")" "$name" "$status" "$headers" "$delay"
}

# stub_route_raw TOKEN NAME STATUS HEADERS_JSON DELAY_MS [USAGE_STATUS]: the same
# for any bearer string; USAGE_STATUS answers GET /api/oauth/usage (default 403).
stub_route_raw() {
  local tmp="$STUB_ROUTES.new"
  jq --arg t "$1" --arg name "$2" --argjson s "$3" --argjson h "$4" --argjson d "$5" --argjson u "${6:-403}" \
    '.[$t] = {name:$name, status:$s, headers:$h, delayMs:$d, usageStatus:$u}' "$STUB_ROUTES" > "$tmp"
  mv "$tmp" "$STUB_ROUTES"
}

# rl_headers FIVE_H_UTIL WEEK_UTIL [5H_STATUS] [7D_STATUS]: rate-limit headers a
# normal reply carries. Utilization is a fraction 0..1; reset is one hour/day out.
rl_headers() {
  local u5="$1" u7="$2" s5="${3:-allowed}" s7="${4:-allowed}" now; now="$(date +%s)"
  jq -n --arg u5 "$u5" --arg u7 "$u7" --arg s5 "$s5" --arg s7 "$s7" \
        --arg r5 "$((now + 3600))" --arg r7 "$((now + 86400))" '{
    "anthropic-ratelimit-unified-5h-utilization": $u5,
    "anthropic-ratelimit-unified-5h-reset": $r5,
    "anthropic-ratelimit-unified-5h-status": $s5,
    "anthropic-ratelimit-unified-7d-utilization": $u7,
    "anthropic-ratelimit-unified-7d-reset": $r7,
    "anthropic-ratelimit-unified-7d-status": $s7,
    "anthropic-ratelimit-unified-status": (if $s5 == "allowed" and $s7 == "allowed" then "allowed" else "rejected" end)
  }'
}

cleanup_world() { stub_stop; }

summary() {
  printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
  if (( FAIL )); then
    printf 'failed:\n'; printf '  %s\n' "${FAILED_NAMES[@]}"
    return 1
  fi
}
