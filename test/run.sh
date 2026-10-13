#!/usr/bin/env bash
# Runs the black-box suite against a built binary:
#
#   1. unit tests for the containment checker      (node --test)
#   2. end-to-end tests for the command line       (test/cli.sh)
#   3. the same end-to-end run again under strace, then every file-changing
#      system call is checked to land inside the throwaway sandbox. This is the
#      proof that a save, a switch, a remove or a rotation writes only inside the
#      temporary HOME the tests hand it.
#
# The binary is $CS_BIN. It must be a build with the test hooks (cargo build
# --release --features test-hooks): they point it at a stand-in for the macOS
# `security` tool and let the Keychain code path run on Linux, so that no test can
# reach a real Keychain. When CS_BIN is not set, this script makes that build in
# target/test-hooks/ (a release build for installing has no hooks and is not
# used here). The Rust unit tests are a separate step: cargo test.
#
# The tests (not the binary) need bash 4+, jq, node 20+ and perl. Step 3 needs
# strace (Linux) and permission to ptrace; without it the script says so and falls
# back to a weaker check (no file in the checkout changed). Set NO_TRACE=1 to skip
# step 3.
#
# Nothing here touches your real home directory: the driver, every test and every
# command under test run with HOME and the XDG_* directories inside .test-tmp/.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# Build the binary under test first, while HOME is still the real one that cargo
# needs for its caches; nothing is run from it until the sandbox below is set up.
if [[ -z "${CS_BIN:-}" ]]; then
  cargo build --locked --release --features test-hooks --target-dir "$ROOT/target/test-hooks"
  CS_BIN="$ROOT/target/test-hooks/release/sharkswap"
fi
export CS_BIN

mkdir -p "$ROOT/.test-tmp"
T="$(mktemp -d "$ROOT/.test-tmp/run.XXXXXX")"
trap 'rm -rf "${T:?}"' EXIT

mkdir -p "$T/tmp" "$T/driver-home" "$T/driver-config" "$T/driver-state" "$T/driver-cache"
export TMPDIR="$T/tmp" HOME="$T/driver-home" \
  XDG_CONFIG_HOME="$T/driver-config" XDG_STATE_HOME="$T/driver-state" XDG_CACHE_HOME="$T/driver-cache"
unset CLAUDE_CONFIG_DIR NODE_OPTIONS NODE_COMPILE_CACHE
export NODE_DISABLE_COMPILE_CACHE=1   # node would otherwise cache compiled code under a path of the caller's choosing

failed=0
step() { printf '\n######## %s\n' "$1"; }

step "unit tests (node --test)"
node --test test/check-writes.test.mjs || failed=1

step "command-line tests (test/cli.sh)"
mkdir -p "$T/plain"
T="$T/plain" bash test/cli.sh || failed=1

if [[ "${NO_TRACE:-}" == 1 ]]; then
  step "containment check skipped (NO_TRACE=1)"
elif command -v strace >/dev/null 2>&1 && strace -f -o /dev/null -e trace=none true 2>/dev/null; then
  step "containment: command-line tests under strace"
  mkdir -p "$T/traced"
  calls="$(node test/check-writes.mjs --calls)"
  # NO_TTY=1: the one test that needs a pseudo-terminal runs in the plain pass only.
  if strace -f -y -qq -s 4096 --seccomp-bpf -e "trace=$calls" -o "$T/trace.txt" \
       env T="$T/traced" NO_TTY=1 bash test/cli.sh >"$T/traced.out" 2>&1; then
    tail -n 1 "$T/traced.out"
  else
    tail -n 25 "$T/traced.out"; failed=1
  fi
  node test/check-writes.mjs --allow "$T" --cwd "$ROOT" "$T/trace.txt" || failed=1

  # Control: the checker must object to a write that does escape the sandbox.
  canary="$ROOT/.test-tmp/escape-canary.$$"
  # shellcheck disable=SC2016  # $1 is for the inner shell
  strace -f -y -qq -s 4096 -e "trace=$calls" -o "$T/control.txt" bash -c ': > "$1"' _ "$canary"
  rm -f "$canary"
  if node test/check-writes.mjs --allow "$T" --cwd "$ROOT" "$T/control.txt" >/dev/null 2>&1; then
    echo "control FAILED: the checker did not flag a write outside the sandbox"; failed=1
  else
    echo "control ok: the checker flags a write outside the sandbox"
  fi
else
  step "containment: strace is not usable here; weaker check"
  echo "note: strace is missing or ptrace is not permitted. Falling back to: no file in the checkout changed."
  mkdir -p "$T/traced"
  : > "$T/marker"
  T="$T/traced" bash test/cli.sh >"$T/traced.out" 2>&1 || { tail -n 25 "$T/traced.out"; failed=1; }
  tail -n 1 "$T/traced.out"
  changed="$(find "$ROOT" -path "$ROOT/.test-tmp" -prune -o -path "$ROOT/.git" -prune -o -newer "$T/marker" -print)"
  if [[ -n "$changed" ]]; then echo "files changed outside the sandbox:"; echo "$changed"; failed=1
  else echo "no file in the checkout changed"; fi
fi

if (( failed )); then printf '\nFAILED\n'; exit 1; fi
printf '\nall tests passed\n'
