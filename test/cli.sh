#!/usr/bin/env bash
# End-to-end tests for the sharkswap command line.
#
# Every scenario builds its own throwaway HOME under $T, fills it with made-up
# logins (alice@example.com and friends, tokens like "fake-access-1"), and
# runs the script with `env -i` so that nothing of the caller's environment or
# real home directory is visible to it. Anything that needs the Anthropic API
# talks to test/stub-api.mjs on a loopback port instead.
#
#   T=/path/under/.test-tmp/run.X bash test/cli.sh
#
# test/run.sh sets T and runs the whole suite. Exit status is 0 when every
# check passed.
set -euo pipefail

: "${T:?run this through test/run.sh, or set T to a directory under .test-tmp}"
# shellcheck source=test/helpers.sh
source "$(dirname "${BASH_SOURCE[0]}")/helpers.sh"
trap cleanup_world EXIT

# A setup-token shaped string, assembled from pieces so that no file in this
# repository contains a literal token prefix. It only ever exists in the sandbox.
setup_token() { printf '%s%s%s' "sk-" "ant-oat01-" "$(printf 'abcdefghij%.0s' 1 2 3 4 5)"; }

# --------------------------------------------------------------------------

s_save_and_list() {
  section "save and list"
  new_world save
  save_as work alice@example.com 1
  assert_rc "save work" 0
  assert_file "metadata snapshot written" "$H/.sharkswap/work.json"
  assert_file "token snapshot written" "$H/.sharkswap/work.creds.json"
  assert_mode "token snapshot is 0600" "$H/.sharkswap/work.creds.json" 600
  assert_mode "metadata snapshot is 0600" "$H/.sharkswap/work.json" 600
  assert_mode "store directory is 0700" "$H/.sharkswap" 700
  assert_eq "metadata holds the account email" "alice@example.com" "$(jq -r .emailAddress "$H/.sharkswap/work.json")"
  assert_eq "token snapshot matches the live login" "$(live_token)" "$(jq -r .claudeAiOauth.accessToken "$H/.sharkswap/work.creds.json")"

  save_as personal bob@example.com 2
  cs list
  assert_rc "list" 0
  assert_out_has "list shows work" "alice@example.com"
  assert_out_has "list shows personal" "bob@example.com"
  local active_line; active_line="$(grep -F 'active' <<<"$OUT" || true)"
  assert_eq "list marks the live login as active" "1" "$(grep -c 'personal' <<<"$active_line" || true)"

  cs save list
  assert_rc "an alias named like a command is refused" 1
  assert_err_has "reserved alias message" "invalid or reserved alias"
  cs save ../escape
  assert_rc "a path-like alias is refused" 1
  cs save .hidden
  assert_rc "a dotted alias is refused" 1
  cs save availability
  assert_rc "the availability cache name is refused" 1
  assert_no_file "nothing written outside the store" "$H/escape.json"
  cs save
  assert_rc "save without an alias fails" 1

  rm -f "$H/.claude/.credentials.json"
  cs save ghost
  assert_rc "save without a live login fails" 1
  assert_err_has "says there is no valid token" "no valid live OAuth token"
  assert_no_file "no half-written snapshot" "$H/.sharkswap/ghost.json"
  assert_eq "no Keychain write was attempted (the stub saw none; on Linux it is never called, see s_keychain_path)" "0" "$(grep -c -e '^-i' -e 'add-generic-password' "$SECURITY_LOG" || true)"
}

s_switch() {
  section "switch"
  new_world switch
  save_as work alice@example.com 1
  save_as personal bob@example.com 2
  jq '.theme = "dark"' "$H/.claude.json" > "$W/x.json" && mv "$W/x.json" "$H/.claude.json"

  ASSUME_YES=0 cs work
  assert_rc "declining the prompt exits non-zero" 1
  assert_eq "a declined switch changes nothing" "bob@example.com" "$(live_email)"

  cs work
  assert_rc "switch to work" 0
  assert_out_has "reports the new account" "Done. Active: alice@example.com"
  assert_eq "live email is work's" "alice@example.com" "$(live_email)"
  assert_eq "live token is work's" "$(fake_token 1)" "$(live_token)"
  assert_eq "the second copy of the metadata is patched too" "alice@example.com" "$(jq -r .oauthAccount.emailAddress "$H/.claude/.claude.json")"
  assert_eq "unrelated settings survive a switch" "dark" "$(jq -r .theme "$H/.claude.json")"
  assert_mode "live credentials are 0600" "$H/.claude/.credentials.json" 600

  cs status
  assert_out_has "status names the alias" "Active alias : work"
  assert_out_has "status names the email" "Active email : alice@example.com"

  cs personal
  assert_eq "switch back to personal" "$(fake_token 2)" "$(live_token)"
  cs next
  assert_rc "next" 0
  assert_eq "next moves round-robin" "alice@example.com" "$(live_email)"

  cs nosuch
  assert_rc "unknown alias is refused" 1
  assert_err_has "says which accounts exist" "unknown account 'nosuch'"
  assert_eq "an unknown alias changes nothing" "alice@example.com" "$(live_email)"

  mk_login carol@example.com 3 1000
  cs save lapsed
  assert_rc "an expired login can still be saved (it refreshes on use)" 0
  assert_out_has "and the expiry is called out" "EXPIRED"
}

# The macOS Keychain path, run on any system: SHARKSWAP_TEST_DARWIN=1 makes
# the binary take it, and a stub stands in for `security` (see helpers.sh), so a
# real Keychain is never reached. The login "in the Keychain" is the file
# $item that the stub hands back for find-generic-password.
s_keychain_path() {
  section "the Keychain path (stub security)"
  new_world keychain
  save_as work alice@example.com 1
  save_as personal bob@example.com 2
  local item="$W/keychain-item" stdin_log="$W/security-stdin.log" count="$W/security-count"
  mv "$H/.claude/.credentials.json" "$item"
  cp "$item" "$W/item.small"
  CS_ENV=(SHARKSWAP_TEST_DARWIN=1 SECURITY_STUB_ITEM="$item" SECURITY_STUB_STDIN="$stdin_log")

  # (a) an ordinary login: one add-generic-password line on stdin, and argv is only -i.
  : > "$SECURITY_LOG"; : > "$stdin_log"
  cs work
  assert_rc "switch to work with the login in the Keychain" 0
  assert_out_has "the token went to the Keychain" "patched token    Keychain 'Claude Code-credentials'"
  assert_eq "live email is work's" "alice@example.com" "$(live_email)"
  assert_eq "security -i was started once, with no other argument" "1" "$(grep -cx -- '-i' "$SECURITY_LOG" || true)"
  assert_eq "no add-generic-password on any argv" "0" "$(grep -c 'add-generic-password' "$SECURITY_LOG" || true)"
  assert_eq "no token on any argv" "0" "$(grep -c 'fake-' "$SECURITY_LOG" || true)"
  assert_eq "exactly one line was sent on stdin" "1" "$(wc -l < "$stdin_log" | tr -d ' ')"
  assert_eq "it is the add-generic-password command for the item" "1" \
    "$(grep -c '^"add-generic-password" "-U" "-s" "Claude Code-credentials" "-a" "tester" "-w" "{' "$stdin_log" || true)"
  assert_eq "and carries work's token" "1" "$(grep -c "$(fake_token 1)" "$stdin_log" || true)"
  assert_eq "with no warning about the process list" "0" "$(grep -c 'briefly visible' "$W/err.txt" || true)"

  # (b) credentials too big for one security -i line: the argv form, and one warning.
  cp "$W/item.small" "$item"
  jq --arg pad "$(head -c 4200 /dev/zero | tr '\0' x)" '. + {mcpOAuth: {pad: $pad}}' "$item" > "$item.big"
  mv "$item.big" "$item"
  : > "$SECURITY_LOG"; : > "$stdin_log"
  cs personal
  assert_rc "switch with credentials over the security -i line limit" 0
  assert_eq "live email is personal's" "bob@example.com" "$(live_email)"
  assert_eq "security -i was not used" "0" "$(grep -cx -- '-i' "$SECURITY_LOG" || true)"
  assert_eq "nothing went on stdin" "0" "$(wc -c < "$stdin_log" | tr -d ' ')"
  assert_eq "one add-generic-password call for the item and the user" "1" \
    "$(grep -c '^add-generic-password -U -s Claude Code-credentials -a tester -w {' "$SECURITY_LOG" || true)"
  assert_eq "it carries personal's token" "1" "$(grep -c "$(fake_token 2)" "$SECURITY_LOG" || true)"
  assert_eq "stderr warns once" "1" "$(grep -c 'briefly visible to other local users in the process list' "$W/err.txt" || true)"

  # (c) a failing security: its stderr is in the error, and the switch changes nothing.
  cp "$W/item.small" "$item"
  CS_ENV=(SHARKSWAP_TEST_DARWIN=1 SECURITY_STUB_ITEM="$item" SECURITY_STUB_STDIN="$stdin_log"
          SECURITY_STUB_FAIL="the stub says: keychain is locked")
  cs work
  assert_rc "a failing security fails the switch" 1
  assert_err_has "the error carries security's own message" "security exited with status 36: the stub says: keychain is locked"
  assert_err_has "and says the write failed" "Keychain write failed"
  assert_out_lacks "no token in the output" "fake-"
  assert_eq "nothing changed" "bob@example.com" "$(live_email)"

  # (c2) the first write works, the metadata commit then fails, and so does the
  # put-back: the run says the previous login was NOT restored and exits non-zero.
  rm -f "$count"
  CS_ENV=(SHARKSWAP_TEST_DARWIN=1 SECURITY_STUB_ITEM="$item" SECURITY_STUB_STDIN="$stdin_log"
          SECURITY_STUB_FAIL="the stub says: keychain is locked" SECURITY_STUB_OK_WRITES=1
          SECURITY_STUB_COUNT="$count" SECURITY_STUB_SABOTAGE="$H/.claude.json")
  cs work
  assert_rc "a switch that cannot be undone exits non-zero" 1
  assert_err_has "it says the previous login was not restored" "the previous login was NOT restored"
  assert_err_has "names the Keychain" "Keychain 'Claude Code-credentials'"
  assert_err_has "and the failed put-back's reason" "the stub says: keychain is locked"
  assert_err_has "and where the copy of the previous file is" "a copy of the previous file is kept at"
  assert_eq "that copy exists" "1" "$(find "$H" -maxdepth 1 -name '.claude.json.backup.*' | wc -l | tr -d ' ')"
  assert_out_lacks "no token in the output" "fake-"
  # The Keychain has no backup file, so the put-back that failed leaves the
  # previous login in a private file in the store, and the message names it.
  local saved="" nsaved
  nsaved="$(find "$H/.sharkswap" -maxdepth 1 -name 'keychain-prior-login.*' | wc -l | tr -d ' ')"
  saved="$(find "$H/.sharkswap" -maxdepth 1 -name 'keychain-prior-login.*' | head -n 1)"
  assert_eq "the previous login was saved in one file in the store" "1" "$nsaved"
  assert_file "the recovery file exists" "$saved"
  assert_mode "and is private" "$saved" 600
  assert_eq "it holds the previous login" "$(cat "$W/item.small")" "$(cat "$saved")"
  assert_err_has "the message names the recovery file" "the previous login is saved in $saved"

  # (d) no login in the Keychain: a save is refused and never writes to it.
  new_world keychain_empty
  save_as work alice@example.com 1
  rm -f "$H/.claude/.credentials.json"
  CS_ENV=(SHARKSWAP_TEST_DARWIN=1)
  : > "$SECURITY_LOG"
  cs save ghost
  assert_rc "save with nothing in the Keychain fails" 1
  assert_err_has "says there is no valid token" "no valid live OAuth token"
  assert_eq "the Keychain was read" "1" "$(grep -c '^find-generic-password -s Claude Code-credentials -w' "$SECURITY_LOG" || true)"
  assert_eq "and not written" "0" "$(grep -c -e '^-i' -e 'add-generic-password' "$SECURITY_LOG" || true)"

  # (e) a login holding non-ASCII text: the real `security -w` prints it as
  # lower-case hex, and a save stores the login itself, not the hex.
  new_world keychain_hex
  mk_login carol@example.com 3
  local note json
  note="$(printf 'caf\303\251 \360\237\224\221')"
  json="$(jq -c --arg n "$note" '.claudeAiOauth.note = $n' "$H/.claude/.credentials.json")"
  rm -f "$H/.claude/.credentials.json"
  { printf '%s' "$json" | od -An -v -tx1 | tr -d ' \n'; echo; } > "$W/keychain-item"
  CS_ENV=(SHARKSWAP_TEST_DARWIN=1 SECURITY_STUB_ITEM="$W/keychain-item")
  cs save carol
  assert_rc "a login that security prints as hex can be saved" 0
  assert_eq "the snapshot holds the decoded login" "$json" "$(jq -c . "$H/.sharkswap/carol.creds.json")"
  assert_eq "with its non-ASCII text intact" "$note" "$(jq -r .claudeAiOauth.note "$H/.sharkswap/carol.creds.json")"
  CS_ENV=()
}

s_pane() {
  section "pane"
  new_world pane
  save_as work alice@example.com 1
  save_as personal bob@example.com 2
  cs pane work
  assert_rc "pane work" 0
  assert_out_has "prints the config dir export" "export CLAUDE_CONFIG_DIR=$H/.sharkswap/homes/work"
  assert_out_has "clears any inherited token variable" "unset CLAUDE_CODE_OAUTH_TOKEN"
  assert_eq "the live login is untouched" "bob@example.com" "$(live_email)"
  assert_file "the pane home has a login" "$H/.sharkswap/homes/work/.credentials.json"
  assert_mode "the pane login is 0600" "$H/.sharkswap/homes/work/.credentials.json" 600
  assert_eq "the pane home logs in as work" "$(fake_token 1)" "$(jq -r .claudeAiOauth.accessToken "$H/.sharkswap/homes/work/.credentials.json")"
  cs pane nosuch
  assert_rc "pane for an unknown alias fails" 1
  assert_eq "no stdout for a failed pane (nothing for eval to run)" "" "$OUT"
}

s_token() {
  section "one-year tokens"
  new_world token
  save_as work alice@example.com 1
  local tok; tok="$(setup_token)"
  printf '%s\n' "$tok" > "$W/tok.txt"

  CS_STDIN="$W/tok.txt" cs token set work
  assert_rc "token set" 0
  assert_out_lacks "token set never prints the token" "$tok"
  assert_file "token file stored" "$H/.sharkswap/tokens/work.json"
  assert_mode "token file is 0600" "$H/.sharkswap/tokens/work.json" 600
  assert_mode "token directory is 0700" "$H/.sharkswap/tokens" 700
  assert_eq "token file holds the token" "$tok" "$(jq -r .token "$H/.sharkswap/tokens/work.json")"

  printf 'not-a-token\n' > "$W/bad.txt"
  CS_STDIN="$W/bad.txt" cs token set work
  assert_rc "a malformed token is refused" 1
  assert_err_has "says what shape it wants" "not a setup-token"
  assert_out_lacks "the refusal does not echo the input" "not-a-token"

  cs token list
  assert_rc "token list" 0
  assert_out_has "list names the alias" "work"
  assert_out_lacks "list never prints the token" "$tok"

  cs pane work
  assert_rc "pane with a token" 0
  # shellcheck disable=SC2016  # the text is literal: the line the pane prints
  assert_out_has "pane exports the token variable" 'CLAUDE_CODE_OAUTH_TOKEN="$('
  assert_out_has "pane asks 'token print' for it" ' token print work)"'
  assert_out_lacks "pane never prints the token" "$tok"
  assert_out_lacks "pane does not shell out to jq" "jq -r"
  local line="$OUT"
  local -a clean=(env -i PATH="$W/bin:$PATH" HOME="$H" USER=tester LANG=C.UTF-8 TERM=dumb TMPDIR="$W/tmp"
    SHARKSWAP_TEST_HOOKS=1 SHARKSWAP_TEST_SECURITY="$W/bin/security")
  local evaluated
  # shellcheck disable=SC2016  # $1 and the variable are for the inner shell
  evaluated="$("${clean[@]}" bash -c 'eval "$1"; echo "${CLAUDE_CODE_OAUTH_TOKEN-UNSET}"' _ "$line")"
  assert_eq "evaluating the pane output sets the token" "$tok" "$evaluated"

  cs token print work
  assert_rc "token print into a pipe" 0
  assert_eq "token print writes the token and a newline" "$tok" "$OUT"
  assert_eq "and nothing on stderr" "" "$ERR"
  if cs_tty token print work; then
    assert_rc "token print on a terminal is refused" 1
    assert_out_has "and says why" "refusing to print a token to a terminal"
    assert_out_lacks "and prints no token" "$tok"
  else
    printf '  skip token print on a terminal (no pseudo-terminal here)\n'
  fi
  cs token print nosuch
  assert_rc "token print for an alias with no token" 1
  assert_eq "prints nothing to stdout" "" "$OUT"
  assert_err_has "says there is no token" "no usable one-year token"
  cs token print ../escape
  assert_rc "token print with a path-like alias is refused" 1
  cs token print
  assert_rc "token print without an alias fails" 1

  # The token goes missing between 'pane' and the eval: the line must say so on
  # stderr and leave the variable unset, never empty or stale.
  mv "$H/.sharkswap/tokens/work.json" "$W/work-token.json"
  # shellcheck disable=SC2016  # $1 and the variable are for the inner shell
  evaluated="$("${clean[@]}" CLAUDE_CODE_OAUTH_TOKEN=stale-value \
    bash -c 'eval "$1"; echo "${CLAUDE_CODE_OAUTH_TOKEN-UNSET}"' _ "$line" 2>"$W/eval-err.txt")"
  assert_eq "a failed token print leaves the variable unset" "UNSET" "$evaluated"
  assert_eq "and the eval says so on stderr" "1" "$(grep -c 'could not read the token for work' "$W/eval-err.txt" || true)"
  mv "$W/work-token.json" "$H/.sharkswap/tokens/work.json"

  stub_start
  stub_route_raw "$tok" work-token 200 "$(rl_headers 0.25 0.35)" 0 200
  cs token check work
  assert_rc "token check" 0
  assert_out_has "token check reads the usage endpoint" "usage endpoint: 200"
  assert_out_lacks "token check never prints the token" "$tok"
  stub_route_raw "$tok" work-token 200 "$(rl_headers 0.25 0.35)" 0 403
  cs token check work
  assert_out_has "a token without the profile scope is explained" "lacks the profile scope"
  : > "$CLAUDE_LOG"
  cs token check work --model
  assert_out_has "token check --model asks claude" "model request: ok"
  assert_eq "the fake claude saw the model request" "1" "$(grep -c -- '--model haiku' "$CLAUDE_LOG" || true)"

  cs probe work
  assert_rc "probe through a token" 0
  assert_eq "the stub saw the token account" "1" "$(grep -c '^POST /v1/messages work-token' "$STUB_LOG" || true)"
  assert_eq "availability records the credential kind" "token" "$(jq -r .work.cred "$H/.sharkswap/availability.json")"
  assert_out_lacks "probe never prints the token" "$tok"
  stub_stop

  cs token rm work
  assert_no_file "token file removed" "$H/.sharkswap/tokens/work.json"
  cs token list
  assert_out_has "list says there are none" "no one-year tokens"
}

s_probe() {
  section "probe"
  new_world probe
  save_as work alice@example.com 1
  save_as personal bob@example.com 2
  save_as spare carol@example.com 3
  save_as broken dave@example.com 4
  stub_start
  stub_route 1 work 200 "$(rl_headers 0.85 0.30)"
  stub_route 2 personal 429 "$(rl_headers 1.0 0.40 rejected allowed)"
  stub_route 3 spare 200 "$(rl_headers 0.10 0.20)"
  # token 4 has no route: the stub answers 401

  cs probe
  assert_rc "probe all" 0
  assert_out_has "table header" "ALIAS"
  assert_eq "work reads 85 and 30" "1" "$(grep -cE '^work +ok +85 +30 ' <<<"$OUT" || true)"
  assert_eq "personal is shown full" "1" "$(grep -cE '^personal +5H FULL' <<<"$OUT" || true)"
  assert_eq "an unauthorised account is shown as such" "1" "$(grep -cE '^broken +AUTH 401' <<<"$OUT" || true)"
  local av="$H/.sharkswap/availability.json"
  assert_mode "availability.json is 0600" "$av" 600
  assert_eq "work is recorded ok" "ok" "$(jq -r .work.state "$av")"
  assert_eq "work 5h use is recorded" "0.85" "$(jq -r '.work.limits["5h"].util' "$av")"
  assert_eq "personal is recorded out for the 5h window" "out/5h" "$(jq -r '.personal | "\(.state)/\(.reason)"' "$av")"
  assert_eq "a refused token is unknown, never out" "unknown" "$(jq -r .broken.state "$av")"
  assert_eq "the stub got one request per account" "4" "$(grep -c '^POST /v1/messages' "$STUB_LOG" || true)"
  assert_eq "requests use the configured probe model" "4" "$(grep -c 'model=claude-haiku-4-5-20251001 beta=oauth-2025-04-20' "$STUB_LOG" || true)"
  assert_out_lacks "no token in the table" "fake-access"

  CS_ENV=(SHARKSWAP_PROBE_MODEL=test-model-x)
  cs probe work
  CS_ENV=()
  assert_eq "SHARKSWAP_PROBE_MODEL is honoured" "1" "$(grep -c 'work model=test-model-x' "$STUB_LOG" || true)"

  cs probe nosuch
  assert_rc "probing an unknown alias fails" 1

  # The token must not be visible to other users through the process table.
  # The request is made inside the sharkswap process: no child process is
  # started for it, and the token is never an argument of any process.
  stub_route 1 work 200 "$(rl_headers 0.85 0.30)" 2500
  cs_bg probe work
  local i seen_proc=0 leaked=0 args
  for i in $(seq 1 40); do
    args="$(ps -eo args= 2>/dev/null || true)"
    if grep -q 'probe work' <<<"$args"; then seen_proc=1; fi
    if grep -q "$(fake_token 1)" <<<"$args"; then leaked=1; fi
    [[ "$seen_proc" == 1 ]] && (( i >= 8 )) && break
    sleep 0.1
  done
  wait "$BG_PID" || true
  assert_eq "saw the probe process while it ran" "1" "$seen_proc"
  assert_eq "the token was never on any command line" "0" "$leaked"
  stub_stop

  # With nothing listening the probe reports "no reply" and records nothing as out.
  cs probe work
  assert_out_has "a dead endpoint is reported as no reply" "no reply"
  assert_eq "no reply never marks an account out" "unknown" "$(jq -r .work.state "$av")"
}

s_gate() {
  section "ceilings"
  new_world gate
  save_as work alice@example.com 1
  save_as personal bob@example.com 2
  save_as spare carol@example.com 3
  stub_start
  stub_route 1 work 200 "$(rl_headers 0.85 0.30)"
  stub_route 2 personal 200 "$(rl_headers 0.10 0.20)"
  stub_route 3 spare 200 "$(rl_headers 0.20 0.20)"
  cs probe
  local caps="$H/.sharkswap/account-caps.json"
  printf '{"caps":{"work":{"5h":80,"7d":90}}}\n' > "$caps"

  cs work
  assert_rc "an account over its ceiling is refused with 75" 75
  assert_err_has "the refusal says why" "5h 85% (ceiling 80%)"
  assert_err_has "the refusal says ASSUME_YES does not lift it" "ASSUME_YES does not lift it"
  assert_eq "a refused switch changes nothing" "carol@example.com" "$(live_email)"

  cs work --force
  assert_rc "--force lifts the refusal" 0
  assert_err_has "and the override is said out loud" "override: --force bypassed the ceiling on work"
  assert_eq "the forced switch happened" "alice@example.com" "$(live_email)"

  cs spare
  assert_rc "an uncapped account is not gated" 0

  cs pane work
  assert_rc "pane honours the ceiling" 75
  assert_eq "a refused pane prints nothing for eval" "" "$OUT"
  cs pane work --force
  assert_rc "pane --force" 0

  cs spare
  cs next
  assert_rc "next" 0
  assert_err_has "next says which account it skipped" "skipping 'work'"
  assert_eq "next did not land on the capped account" "0" "$([[ "$(live_email)" == alice@example.com ]] && echo 1 || echo 0)"

  cs best
  assert_err_has "best explains the ceiling" "cap: work is at its ceiling"

  # Saving a second name for a capped login would dodge its ceiling.
  mk_login alice@example.com 1
  cs save work-copy
  assert_rc "saving a capped login under another name is refused" 75
  assert_err_has "and the holder is named" "already saved as 'work'"
  assert_no_file "no copy was made" "$H/.sharkswap/work-copy.json"
  cs save work-copy --force
  assert_rc "save --force" 0
  cs remove work-copy

  rm -f "$H/.sharkswap/availability.json"
  cs work
  assert_rc "a capped account with no reading is refused" 75

  echo 'not json' > "$caps"
  cs personal
  assert_rc "an unreadable caps file refuses every account" 75
  assert_err_has "and says so" "UNUSABLE"
  cs personal --force
  assert_rc "--force still works then" 0

  rm -f "$caps"
  cs work
  assert_rc "no caps file means no ceilings" 0
  stub_stop
}

s_best_and_bar() {
  section "best and bar"
  new_world best
  save_as alpha a@example.com 1
  save_as bravo b@example.com 2
  save_as gamma c@example.com 3
  stub_start
  stub_route 1 alpha 200 "$(rl_headers 0.10 0.20)"
  stub_route 2 bravo 200 "$(rl_headers 0.50 0.60)"
  stub_route 3 gamma 429 "$(rl_headers 1.0 0.40 rejected allowed)"
  cs probe

  cs best --raw
  assert_rc "best --raw" 0
  local order states
  order="$(cut -f1 <<<"$OUT" | tr '\n' ' ')"
  states="$(cut -f5 <<<"$OUT" | tr '\n' ' ')"
  assert_eq "least used first, blocked last" "alpha bravo gamma " "$order"
  assert_eq "states" "fresh fresh BLOCKED " "$states"
  assert_eq "week percentage column" "20 60 40" "$(cut -f2 <<<"$OUT" | tr '\n' ' ' | sed 's/ $//')"
  assert_eq "5h percentage column" "10 50 100" "$(cut -f6 <<<"$OUT" | tr '\n' ' ' | sed 's/ $//')"

  cs best
  assert_out_has "best table" "ALIAS"
  assert_out_has "best lists the blocked account" "BLOCKED"

  cs best --switch
  assert_rc "best --switch" 0
  assert_eq "moved to the top usable account" "a@example.com" "$(live_email)"

  cs bar alpha
  assert_eq "bar for alpha" "alpha · 5h 10% · wk 20%" "$OUT"
  cs bar gamma
  assert_out_has "bar shows a spent window" "gamma · 5h 100% · wk 40%"
  cs bar
  assert_out_has "bar defaults to the active account" "alpha"

  rm -f "$H/.sharkswap/availability.json"
  cs bar bravo
  assert_out_has "bar without a reading says so" "no reading"
  stub_stop
}

s_rotate() {
  section "rotate"
  new_world rotate
  save_as work alice@example.com 1
  save_as personal bob@example.com 2
  stub_start
  stub_route 1 work 429 "$(rl_headers 1.0 0.40 rejected allowed)"
  stub_route 2 personal 200 "$(rl_headers 0.10 0.20)"
  cs work
  mkdir -p "$W/config/sharkswap"
  printf '# pool for the tests\npersonal\nwork\n' > "$W/config/sharkswap/pool"
  local log="$W/state/sharkswap-rotate.log" st="$W/state/sharkswap-rotate.json"

  cs rotate --dry-run
  assert_rc "dry run" 0
  assert_out_has "dry run says what it would do" "switch: work"
  assert_out_has "and to whom" "-> personal"
  assert_eq "a dry run changes nothing" "alice@example.com" "$(live_email)"
  assert_no_file "a dry run writes no state" "$st"
  assert_no_file "a dry run writes no log" "$log"

  # A lock held by a live process: the run must skip, and say so. The lock is an
  # advisory lock on the lock file; perl holds one for a few seconds.
  perl -e 'use Fcntl qw(:flock); open(my $f, ">>", $ARGV[0]) or die; flock($f, LOCK_EX) or die; $| = 1; print "held\n"; sleep 30' "$st.lock" >"$W/held.txt" &
  local holder=$! n
  for n in $(seq 1 100); do [[ -s "$W/held.txt" ]] && break; sleep 0.05; done
  assert_eq "the test holds the lock" "held" "$(cat "$W/held.txt")"
  cs rotate
  assert_rc "a run beside a live lock exits 0" 0
  assert_eq "and does not switch" "alice@example.com" "$(live_email)"
  assert_eq "the log says another run holds the lock" "1" "$(grep -c 'another run holds' "$log" || true)"
  kill "$holder" 2>/dev/null || true
  wait "$holder" 2>/dev/null || true
  # The file is left behind, but nothing holds it any more: it blocks no one.

  # A lock file that cannot be opened (here a directory in its place): the run
  # is skipped, with the fault and its errno in the log, and it fails with 75
  # (EX_TEMPFAIL) so that a timer or a script sees that nothing ran. A lock that
  # is merely busy (above) is not a failure.
  rm -f "$st.lock"
  mkdir "$st.lock"
  cs rotate
  assert_rc "a run that cannot take the lock exits 75" 75
  assert_eq "and does not switch" "alice@example.com" "$(live_email)"
  assert_eq "the log says the lock could not be taken" "1" "$(grep -c 'cannot take the lock' "$log" || true)"
  assert_eq "and names the failing step and its errno" "1" "$(grep -c 'cannot open .*\.lock: Is a directory (errno 21)' "$log" || true)"
  assert_err_has "stderr says it too" "cannot take the lock"
  cs rotate --dry-run
  assert_rc "a dry run takes no lock, so it still plans" 0
  assert_out_has "and says what it would do" "switch: work"
  rmdir "$st.lock"

  : > "$CLAUDE_LOG"
  cs rotate
  assert_rc "rotate" 0
  assert_eq "the current account was refused, so the login moved" "bob@example.com" "$(live_email)"
  assert_eq "the log has the decision" "1" "$(grep -c 'switch: work' "$log" || true)"
  assert_eq "and the verification" "1" "$(grep -c 'now on personal; verified' "$log" || true)"
  assert_eq "state records the switch" "personal" "$(jq -r .alias "$st")"
  assert_eq "state has a switch time" "number" "$(jq -r '.last_switch | type' "$st")"
  assert_mode "state file is 0600" "$st" 600
  assert_eq "a snapshot target gets a warm-up request" "1" "$(grep -c -- '-p reply OK --model claude-haiku-4-5-20251001' "$CLAUDE_LOG" || true)"
  assert_no_file "the lock is released" "$st.lock"

  local before; before="$(wc -l < "$log")"
  cs rotate
  assert_rc "rotate again" 0
  assert_eq "the healthy account stays put" "bob@example.com" "$(live_email)"
  assert_eq "and a quiet run logs nothing" "$before" "$(wc -l < "$log")"

  # Now the new account is refused too. The switch gap holds the next move back.
  stub_route 2 personal 429 "$(rl_headers 1.0 0.20 rejected allowed)"
  stub_route 1 work 200 "$(rl_headers 0.05 0.40)"
  cs rotate
  assert_eq "inside the minimum gap the login stays" "bob@example.com" "$(live_email)"
  cs rotate --min-gap 0
  assert_rc "with the gap lifted" 0
  assert_eq "it moves back" "alice@example.com" "$(live_email)"

  cs rotate --min-gap x
  assert_rc "a bad --min-gap is a usage error" 2
  cs rotate --nonsense
  assert_rc "an unknown flag is a usage error" 2

  rm -f "$W/config/sharkswap/pool"
  cs rotate
  assert_err_has "a missing pool is reported" "the pool is empty"
  stub_stop
}

s_remove() {
  section "remove"
  new_world remove
  save_as work alice@example.com 1
  save_as personal bob@example.com 2
  local tok; tok="$(setup_token)"
  printf '%s\n' "$tok" > "$W/tok.txt"
  CS_STDIN="$W/tok.txt" cs token set work
  cs pane work
  assert_file "pane home exists before remove" "$H/.sharkswap/homes/work"
  cs remove work
  assert_rc "remove" 0
  assert_no_file "metadata gone" "$H/.sharkswap/work.json"
  assert_no_file "login snapshot gone" "$H/.sharkswap/work.creds.json"
  assert_no_file "token gone" "$H/.sharkswap/tokens/work.json"
  assert_no_file "pane home gone" "$H/.sharkswap/homes/work"
  assert_file "other accounts stay" "$H/.sharkswap/personal.json"
  cs list
  assert_out_lacks "list no longer shows it" "alice@example.com"
  cs remove nosuch
  assert_rc "removing an unknown alias fails" 1
}

s_overview_and_status() {
  section "overview and status"
  new_world overview
  save_as work alice@example.com 1
  save_as personal bob@example.com 2
  cs
  assert_rc "bare command" 0
  assert_out_has "overview names the active account" "Active alias : personal"
  assert_out_has "overview ranks the rest" "ALIAS"

  # Without QUICK=1 status reads the active account's usage over the network.
  stub_start
  stub_route 2 personal 200 "$(rl_headers 0.33 0.44)"
  QUICK=0 cs status
  assert_rc "status with a probe" 0
  assert_eq "status probed the active account once" "1" "$(grep -c '^POST /v1/messages personal' "$STUB_LOG" || true)"
  assert_eq "and recorded it" "0.33" "$(jq -r '.personal.limits["5h"].util' "$H/.sharkswap/availability.json")"
  cs status
  assert_eq "QUICK=1 does not touch the network" "1" "$(grep -c '^POST' "$STUB_LOG" || true)"
  stub_stop
}

s_install() {
  section "install.sh"
  new_world install
  # Install from a copy inside the sandbox, with the binary under test.
  local co="$W/checkout"
  mkdir -p "$co"
  cp "$ROOT/install.sh" "$co/"
  local bin="$W/prefix/bin"
  local run=(env -i PATH="$PATH" HOME="$H" TMPDIR="$W/tmp")

  # The binary under test has the test hooks, so install.sh must refuse it.
  local rc=0
  "${run[@]}" bash "$co/install.sh" --binary "$CS_BIN" --prefix "$bin" >"$W/out.txt" 2>&1 || rc=$?
  assert_eq "a binary built with the test hooks is refused" "1" "$rc"
  assert_eq "and the reason is named" "1" "$(grep -c 'built with the test hooks' "$W/out.txt" || true)"
  assert_no_file "nothing was installed" "$bin/sharkswap"
  assert_no_file "no link either" "$bin/cs"

  # A stand-in for a release build: a script without the hook names that runs the
  # binary under test (copying and patching the binary itself would break its
  # code signature on a Mac). It answers `help` like the tool, so install.sh takes it.
  local release="$W/release-sharkswap"
  printf '#!/bin/sh\nexec %q "$@"\n' "$CS_BIN" > "$release"
  chmod +x "$release"
  assert_eq "the stand-in holds no hook name" "0" "$(LC_ALL=C grep -ac SHARKSWAP_TEST_SECURITY "$release" || true)"
  "${run[@]}" bash "$co/install.sh" --binary "$release" --prefix "$bin" >"$W/out.txt" 2>&1 || true
  assert_file "sharkswap is installed" "$bin/sharkswap"
  assert_eq "as a copy, not a link into the build directory" "0" "$([[ -L "$bin/sharkswap" ]] && echo 1 || echo 0)"
  assert_eq "cs is a link to it" "sharkswap" "$(readlink "$bin/cs")"

  save_as work alice@example.com 1
  CS_PROG="$bin/cs" cs list
  assert_rc "the short name works through the link" 0
  assert_out_has "and sees the store" "alice@example.com"
  CS_PROG="$bin/sharkswap" cs best --raw
  assert_rc "the long name works" 0

  "${run[@]}" bash "$co/install.sh" --binary "$release" --prefix "$bin" >"$W/out.txt" 2>&1 || true
  assert_eq "a second run reports both as present" "2" "$(grep -c 'already' "$W/out.txt" || true)"

  "${run[@]}" bash "$co/install.sh" --uninstall --prefix "$bin" >"$W/out.txt" 2>&1 || true
  assert_no_file "uninstall removes sharkswap" "$bin/sharkswap"
  assert_no_file "uninstall removes cs" "$bin/cs"
  assert_file "the binary it installed from is left alone" "$release"

  mkdir -p "$W/other"; : > "$W/other/cs"; printf '#!/bin/sh\necho mine\n' > "$W/other/sharkswap"; chmod +x "$W/other/sharkswap"
  "${run[@]}" bash "$co/install.sh" --binary "$release" --prefix "$W/other" >"$W/out.txt" 2>&1 || true
  assert_eq "a foreign file is never replaced" "0" "$([[ -L "$W/other/cs" ]] && echo 1 || echo 0)"
  assert_eq "nor a foreign program of the same name" "mine" "$("$W/other/sharkswap")"
  "${run[@]}" bash "$co/install.sh" --uninstall --prefix "$W/other" >"$W/out.txt" 2>&1 || true
  assert_file "uninstall leaves a foreign program alone" "$W/other/sharkswap"
  assert_file "and a foreign file" "$W/other/cs"
}

# A switch, a save, a remove and a rotation write only inside the temporary HOME.
# Traced with strace when it is usable (and this run is not already traced, as it
# is under test/run.sh, whose own trace covers the whole suite); otherwise the
# weaker check: no file in the checkout, nor in any other sandbox, changed.
s_contained() {
  section "writes stay inside the sandbox"
  new_world contained
  save_as work alice@example.com 1
  save_as personal bob@example.com 2
  mkdir -p "$W/config/sharkswap"
  printf '%s\n' work personal > "$W/config/sharkswap/pool"
  : > "$W/marker"

  local tracer=0 n=0
  if command -v strace >/dev/null 2>&1 && grep -q '^TracerPid:[[:space:]]*0$' /proc/self/status 2>/dev/null \
     && strace -f -o /dev/null -e trace=none true 2>/dev/null; then tracer=1; fi
  traced_cs() {
    if (( tracer )); then
      n=$((n + 1))
      CS_WRAP=(strace -f -y -qq -s 4096 --seccomp-bpf -e "trace=$(node "$ROOT/test/check-writes.mjs" --calls)" -o "$W/trace.$n.txt")
    fi
    cs "$@"
    CS_WRAP=()
  }

  traced_cs work
  assert_rc "switch to work" 0
  assert_eq "the live login changed" "alice@example.com" "$(live_email)"
  traced_cs personal
  traced_cs next
  traced_cs save extra
  traced_cs remove extra
  traced_cs rotate --dry-run
  traced_cs token list
  assert_eq "next wrapped round to work" "alice@example.com" "$(live_email)"

  if (( tracer )); then
    cat "$W"/trace.*.txt > "$W/trace.all"
    if node "$ROOT/test/check-writes.mjs" --allow "$W" --cwd "$PWD" "$W/trace.all" > "$W/check.out" 2>&1; then
      pass "strace saw no write outside the sandbox ($(head -n 1 "$W/check.out"))"
    else
      fail "strace saw a write outside the sandbox" "$(head -n 6 "$W/check.out")"
    fi
  else
    local stray
    stray="$(find "$ROOT" \( -path "$ROOT/.test-tmp" -o -path "$ROOT/.git" \) -prune -o -newer "$W/marker" -print
             find "$T" -path "$W" -prune -o -newer "$W/marker" -print)"
    assert_eq "no file outside this sandbox changed (no strace here)" "" "$stray"
  fi
}

s_help_and_dispatch() {
  section "help and command names"
  new_world help
  cs help
  assert_rc "help" 0
  assert_out_has "help has a usage line" "sharkswap rotate"
  local help_out="$OUT"
  cs --help
  assert_eq "--help prints the same" "$help_out" "$OUT"
  cs -h
  assert_eq "-h prints the same" "$help_out" "$OUT"
  assert_out_lacks "help stops at the end of the comment block" "set -euo pipefail"

  # Every word the dispatch answers as a command is refused as an account name
  # (the Rust test dispatch_table_matches_the_reserved_words keeps the table and
  # the list in step; this checks the refusal end to end).
  save_as work alice@example.com 1
  local word bad=""
  for word in next toggle status st list ls pane save watch best bar token probe rotate remove rm help -h --help; do
    cs save "$word"
    if [[ "$RC" != 1 || "$ERR" != *"invalid or reserved alias"* ]]; then bad="$bad $word"; fi
  done
  assert_eq "every command word is a reserved alias" "" "$bad"
  cs save work2
  assert_rc "an ordinary name is not" 0
}

# A symbolic link as the store, readable by others: the warning about it shows for
# an ordinary command but never from the status line or the watcher.
s_symlinked_store() {
  section "a symbolic link as the store"
  new_world symlinked
  mkdir -p "$W/elsewhere"
  chmod 755 "$W/elsewhere"
  ln -s "$W/elsewhere" "$H/.sharkswap"
  local warning="is a symbolic link to a directory readable by other users"
  cs list
  assert_rc "list with the store behind a link" 0
  assert_err_has "list warns about the link" "$warning"
  assert_mode "the directory it points to keeps its mode" "$W/elsewhere" 755
  cs bar
  assert_out_lacks "bar says nothing about it" "$warning"
  CS_ENV=(WATCH_INTERVAL=bad)
  cs watch
  assert_rc "watch with a bad interval stops at once" 1
  assert_out_lacks "watch says nothing about it" "$warning"
  CS_ENV=()
}

for scenario in s_save_and_list s_switch s_keychain_path s_pane s_token s_probe s_gate s_best_and_bar s_rotate s_remove s_overview_and_status s_symlinked_store s_install s_contained s_help_and_dispatch; do
  "$scenario"
done
summary
