# sharkswap

[![CI](https://github.com/spicysharkco/sharkswap/actions/workflows/ci.yml/badge.svg)](https://github.com/spicysharkco/sharkswap/actions/workflows/ci.yml)
[![License: PolyForm Noncommercial 1.0.0](https://img.shields.io/badge/license-PolyForm%20Noncommercial%201.0.0-blue)](LICENSE)

A Claude Code account switcher. Switch (swap) between several Claude Code
accounts from the command line, see how much of each account's 5-hour and weekly
allowance is used, and (optionally) let a timer move your login to another
account when the current one runs out.

It is a single binary written in Rust. There is no daemon and no service. The
program needs no shell libraries, no `jq`, no `curl` and no Node. It does talk to
other programs and to the network in a few places, and all of them are listed here:

- Usage: `cs probe` (and `cs`, `cs status`, `cs best`, `cs bar` unless `QUICK=1`) make
  one one-token HTTPS request per account, from inside the process.
- `cs token check` makes one more HTTPS request, to the usage endpoint, and runs
  `claude --version`. With `--model` it also runs `claude -p`, which makes a real model
  request through Claude Code.
- `cs rotate` runs `claude -p` once after it moves to a saved login, to warm that
  login up. That is a network request made by Claude Code, not by this program.
- On macOS, where Claude Code keeps its login in the Keychain, the program runs the
  system `security` tool (`/usr/bin/security`, by absolute path): once per `cs watch` tick (every 30 seconds by default) while
  the login is Keychain-only, and when a command reads or writes the login.

Unofficial; not affiliated with Anthropic.

sharkswap is beta (0.x): commands, output and file formats may still change
before 1.0. The [changelog](CHANGELOG.md) lists every change.

Use responsibly: follow the terms of each account you use.

## What it does

Claude Code decides which account it bills from the OAuth token in
`~/.claude/.credentials.json`. The `oauthAccount` block in `~/.claude.json` is only
the name it shows. Swapping the name without the token changes nothing real, so
`sharkswap` swaps both.

- Saves each account you log in to as a named alias (a snapshot of the token and the
  display block).
- Switches the active login to any saved alias (`cs work`), or round-robin (`cs next`).
- Prints a parallel-terminal setup (`cs pane work`) so two accounts can run in two
  terminals at once, each in its own config directory.
- Shows 5-hour and weekly use per account, read from the rate-limit headers of a
  one-token request (`cs probe`, `cs`, `cs best`).
- Ranks accounts by remaining room (`cs best`) and can switch to the best one.
- Keeps per-account **ceilings**: stop using an account at, say, 80% of its 5-hour
  window, well before the provider refuses it.
- Runs **rotation** from a timer: use an account until it is refused (or hits its
  ceiling), then move to the account with the most room.
- Stores optional one-year tokens from `claude setup-token`, which do not need the
  monthly re-login.

## Install

### Prebuilt binary

No Rust toolchain is needed. From the
[Releases page](https://github.com/spicysharkco/sharkswap/releases/latest), download
the archive for your system and the `SHA256SUMS` file:

| System | Archive |
|---|---|
| macOS, Apple silicon | `sharkswap-VERSION-aarch64-apple-darwin.tar.gz` |
| macOS, Intel | `sharkswap-VERSION-x86_64-apple-darwin.tar.gz` |
| Linux, x86-64 | `sharkswap-VERSION-x86_64-unknown-linux-musl.tar.gz` |
| Linux, ARM64 | `sharkswap-VERSION-aarch64-unknown-linux-musl.tar.gz` |

The Linux binaries are statically linked, so they run on any distribution. Check the
archive, unpack it and install. The example is version 0.3.0 for Apple silicon; set `a`
to the archive name for your version and system from the table above, without
`.tar.gz`:

```sh
a=sharkswap-0.3.0-aarch64-apple-darwin
grep -F " $a.tar.gz" SHA256SUMS | shasum -a 256 -c -
gh attestation verify "$a.tar.gz" --repo spicysharkco/sharkswap \
  --signer-workflow spicysharkco/sharkswap/.github/workflows/release.yml \
  --source-ref refs/tags/v0.3.0
tar -xzf "$a.tar.gz"
xattr -dr com.apple.quarantine "$a"
cd "$a"
./install.sh --binary ./sharkswap
```

On Linux, check with `sha256sum -c -` in place of `shasum -a 256 -c -`, and skip
`xattr`: it is only needed on macOS after a browser download (see below). `shasum` and
`sha256sum` check that the download is intact and print `OK`; `grep` picks the line for
your archive, so this also works with BusyBox. `gh attestation verify`
(GitHub CLI) checks that this repository's release workflow, run for the tag you
downloaded, built the archive; it reads the attestation through the GitHub API, so run
`gh auth login` first. `install.sh` copies the binary to `~/.local/bin` and adds a `cs`
link; it takes `--prefix DIR` and `--uninstall` as described below. To install by hand
instead, run `mkdir -p ~/.local/bin`, copy `sharkswap` there (that directory must be on
your `PATH`) and link `cs` next to it: `ln -s sharkswap ~/.local/bin/cs`. If you use
`cargo-binstall`, `cargo binstall --git https://github.com/spicysharkco/sharkswap sharkswap`
clones the repository, reads the version from `Cargo.toml` on the default branch and downloads that
version's archive (so it can be newer than the latest release; if that version has no
archive, binstall builds from source). It installs `sharkswap` only, with no `cs` link,
and checks neither `SHA256SUMS` nor the attestation.

The macOS binaries are not notarised by Apple. A browser download is quarantined, the
flag follows the files through `tar`, `cp` and `install`, and Gatekeeper then refuses to
run the binary ("Apple could not verify “sharkswap” is free of malware that may harm
your Mac or compromise your privacy" on macOS 15 and later, "cannot be opened because
the developer cannot be verified" before). `install.sh` runs the binary to check it, so
it would stop with "not a sharkswap binary". The `xattr` line between `tar` and `cd`
clears the flag on the unpacked directory first. A download made with `curl` or
`gh release download` is not flagged, and that line changes nothing.

### With cargo

You need Rust 1.89 or newer (`rustup` is the easy way to get it).

```sh
cargo install --locked --git https://github.com/spicysharkco/sharkswap
```

That puts `sharkswap` in `~/.cargo/bin`. `--locked` builds with the dependency
versions in the repository's `Cargo.lock`, the ones the tests ran against. Or build
from source:

```sh
git clone https://github.com/spicysharkco/sharkswap
cd sharkswap
cargo build --release --locked
./install.sh
```

The release build has no test hooks (see Tests); it is the build to install.
`./install.sh` copies the binary to `~/.local/bin` and adds a `cs` link.
`./install.sh --prefix DIR` installs into another directory instead, and
`./install.sh --uninstall` removes only what it installed.

`cs` is only a shorthand. It is a symlink to `sharkswap`, made by `install.sh`
(or by hand: `ln -s sharkswap ~/.cargo/bin/cs`); the program behaves the same
under either name. `install.sh` never replaces a file it did not install. Make sure
the bin directory is on your `PATH`. The examples below use `cs`.

Claude Code (`claude`) must be installed and logged in at least once. Linux and macOS
are supported; there is no Windows support.

## Quick start

A token can only be captured while its account is the active login, so build the
store by logging in to each account once.

1. In Claude Code, run `/login` as your first account, then `cs save work`.
2. Run `/login` as the second account, then `cs save personal`.
3. `cs` on its own shows them all.
4. Switch with `cs work` or `cs personal`, or with `cs next`, which goes round-robin
   and skips any account over its ceiling.

After a switch, start a new `claude` session. A session that is already running may
keep the account it started with.

Two accounts in two terminals at once: after `eval "$(cs pane work)"`, this shell uses
`work`, in its own config directory.

```sh
eval "$(cs pane work)"
claude
```

## Commands

| Command | What it does |
|---|---|
| `cs` | Overview: the active account, then every saved account with 5-hour and weekly use and reset times. Probes unless `QUICK=1`. |
| `cs <alias> [--force]` | Switch the live login to `alias` (token and display block). Refused with exit 75 when the account is over a ceiling, unless `--force`. |
| `cs next` | Switch to the next saved account, round-robin, skipping accounts a ceiling refuses. |
| `cs pane <alias> [--force]` | Print `export CLAUDE_CONFIG_DIR=...` (and a line that sets `CLAUDE_CODE_OAUTH_TOKEN` when the alias has a one-year token) for a parallel terminal. Use with `eval "$(cs pane alias)"`. |
| `cs status` | Current account, token and login expiry, live usage (`QUICK=1` skips the probe). |
| `cs list` | Saved accounts, with a mark for a captured token and for an expired login. |
| `cs save <alias> [--force]` | Snapshot the current login as `alias`. Refused when this login is already saved under an alias that has a ceiling, unless `--force`. |
| `cs watch` | Run in the foreground and re-save the active token whenever it changes (Claude Code refreshes tokens). |
| `cs best [--switch\|--raw]` | Rank saved accounts, most usable first. `--switch` moves to the top usable one. `--raw` prints tab-separated rows. |
| `cs bar [alias]` | One status-bar line: `alias · 5h N% · wk N%`, plus age, login time left and any cap. |
| `cs probe [alias...]` | Send one 1-token request per account and record 5-hour and weekly use from the response headers. |
| `cs token set\|rm\|list\|check\|print <alias>` | Manage one-year tokens from `claude setup-token`. `set` reads the token from stdin. `check` asks the usage endpoint whether the token can read quota, and with `--model` sends one request through `claude -p`. `print` writes the token to standard output, only when that is not a terminal (see below). |
| `cs remove <alias>` | Delete a saved account, its token and its pane directory. |
| `cs rotate [options]` | One rotation run, meant for a timer (see Rotation). |
| `cs help` | Usage. |
| `cs --version` | Print the version on one line, `sharkswap X.Y.Z`. `-V` does the same. Like `cs help`, it reads no login and makes no files. |

The command words above, and `-h`, `--help`, `--version` and `-V`, are reserved, so they cannot be used as aliases.

`best --raw` columns: alias, weekly %, weekly reset (ISO time), age in days, state,
5-hour %. States: `fresh`, `PROBE-OK`, `PROBE-?`, `PROBE-ERR`, `BLOCKED`, `SPENT`,
`STALE`, `EXPIRED`.

When an alias has a one-year token, `cs pane` prints a second line, of the form

```sh
CLAUDE_CODE_OAUTH_TOKEN="$(/path/to/sharkswap token print work)" && export CLAUDE_CODE_OAUTH_TOKEN || { echo '...' >&2; unset CLAUDE_CODE_OAUTH_TOKEN; }
```

so the token itself never appears in the output, the terminal scrollback or your shell
history. When the line is evaluated, `cs token print <alias>` writes the token to the
pipe behind `$(...)`. On a terminal it refuses (message on stderr, exit 1), so a stray
`cs token print work` cannot leave a token in your scrollback. If the token cannot be
read, the line says so on stderr and leaves `CLAUDE_CODE_OAUTH_TOKEN` unset, rather
than leaving an empty or an old value. `jq` is not used anywhere in the program.

## Ceilings

A ceiling keeps some headroom on an account you share, or only mean to spend part of.
Create `~/.sharkswap/account-caps.json` (an example is in
`share/account-caps.example.json`):

```json
{
  "caps": {
    "work":     { "5h": 80, "7d": 90 },
    "personal": { "7d": 95 }
  }
}
```

A ceiling is a percent from 1 to 100, for the 5-hour window (`5h`), the weekly window
(`7d`), or both. Percentages are the ones `cs probe` read from the headers, rounded to
a whole number the way they are shown.

- At or over a ceiling, `cs <alias>` and `cs pane <alias>` exit 75 and print why.
  `--force` goes ahead anyway.
- A capped account with no current reading is refused too, because it cannot be shown
  to be under its ceiling. Run `cs probe <alias>` first.
- `cs next`, `cs best --switch` and rotation skip accounts the ceiling refuses.
- Two aliases saved from the same login share the ceiling.
- No file means no ceilings. A file that cannot be used at all (not JSON, no `caps`
  object) refuses every alias until it is fixed, so a typo cannot silently drop a
  ceiling. One bad entry only makes that alias refuse.

## Rotation

`cs rotate` does one run and exits. Run it every ten minutes from cron, a systemd user
timer or a launchd job:

```cron
*/10 * * * * $HOME/.local/bin/cs rotate
```

Copy `share/rotate-pool.example` to `~/.config/sharkswap/pool` and list the aliases
this machine may move to (whitespace separated, `#` starts a comment). Options:
`--pool FILE`, `--log FILE`, `--state FILE`, `--min-gap SEC` (default 900), `--tick SEC`
(default 600, your timer interval), and `--dry-run` (print what it would do, change
nothing). The log is `~/.local/state/sharkswap-rotate.log` and the state file next
to it; both follow `XDG_STATE_HOME`.

The rules:

1. Use an account until the provider refuses it (its 5-hour or weekly status is
   `rejected`), then move. There is no percentage threshold, because the header
   figures read a few points higher than the usage page, and a threshold would leave
   accounts that still have room.
2. An account with a ceiling is left once it reaches the ceiling, and a little early
   when the recent burn rate would pass it before the next run.
3. At most one switch per `--min-gap`. Leaving a capped account never waits.
4. An account that cannot be read is not left on the first failed request. It takes two
   unreadable runs in a row, or a refused credential (401 or 403) at once.
5. The target is the pool account with the lowest of (5-hour, weekly) use, then the
   lower 5-hour use, then pool order. Blocked accounts, and capped accounts without a
   current reading, are skipped. A target whose switch just failed is tried last.
6. If the pool has nothing open and the current account is at its ceiling, a one-year
   token account that is under its own ceiling may be used even though it is outside
   the pool.
7. A stored login whose access token expired cannot be probed. It is used only as a
   last resort, and only when something else answered this run.

Notes (an entry in the ceilings file that is ignored or kept refused; a host login
that is not the current account's one-year token and can no longer refresh) go to
stderr on every run, but
a real run logs a note only when it begins (`note: ...`) and when it stops (`note
ended: ...`), against the `notes` the previous real run kept in the state file. A
standing note would otherwise fill the log every ten minutes and bury the switches.
A note spanning several lines is logged a line each, blank lines dropped. If the log
cannot take a line, the notes are not remembered and the next run logs what is still
owed. A dry run changes neither the log nor that memory. A run stopped by an unusable
ceilings file leaves the state alone, so it logs every note again.

Only one run happens at a time: a run takes an exclusive lock on a file next to the
state file. A second run that finds it taken is not an error: it logs that another run
is active and exits 0. A run that cannot take the lock at all (the file or its
directory cannot be created or opened, or the lock call fails) did not run, so it logs
why, with the system error and its number, and exits 75 (`EX_TEMPFAIL`), which a timer
or a script can tell apart from a run that finished. The operating system drops the
lock when the run ends, however it ends, so a crashed run never leaves a stale lock
behind. The switch itself is `cs <target> --force` run non-interactively, followed by a
check that the live login changed.

## Where things are stored

Everything lives under `~/.sharkswap/` (directory mode 0700, files 0600, `umask 077`):

| Path | Contents |
|---|---|
| `<alias>.json` | The `oauthAccount` display block for the alias. |
| `<alias>.creds.json` | The `claudeAiOauth` token block (OAuth access and refresh tokens). |
| `tokens/<alias>.json` | An optional one-year token: `{"token": "...", "created": <epoch>}`. |
| `homes/<alias>/` | The config directory `cs pane` hands to a parallel terminal. |
| `availability.json` | The latest probe reading per alias. |
| `account-caps.json` | Your ceilings. |
| `.identity.lock/` | Held while a switch, save or remove runs. After an interrupted switch it stays and holds `switch-pending` (see below). |

Tokens are plain files protected by file permissions only. They are not encrypted. Do
not put the directory in a synced folder or a backup you do not control.

Tokens are not put on a command line (which any user on the machine can read with
`ps`), with one exception on macOS that is described below and warned about when it
happens, and the usage request is made inside the process: it starts no child for it.
On macOS, where Claude Code keeps its login in the Keychain, the program reads and
writes the Keychain item `Claude Code-credentials` through the system `security` tool,
which it runs as `/usr/bin/security` (an absolute path, so a `security` earlier in
`PATH` is never used); that needs a logged-in GUI session and does not work over plain
SSH. A write feeds `add-generic-password` to `security -i` on its standard input, so
the credentials JSON is normally not in the tool's arguments; `/usr/bin/security`
stays the one that creates the item, so the item keeps the access rules Claude Code
expects. A token that a child needs goes in its environment, not its arguments
(`token check --model` hands the token to `claude` that way).

`security -i` reads one line of at most 4094 bytes, not counting the newline. The line
`cs` sends is

```
"add-generic-password" "-U" "-s" "SERVICE" "-a" "ACCOUNT" "-w" "JSON"
```

with every word in double quotes and each `"` and `\` inside a word preceded by a `\`
(so each takes two bytes). The 4094 bytes are for that whole quoted line, not for the
JSON alone: the fixed words, the quotes, and the escaping of every quote and backslash
in the JSON all count. If
the line fits, `cs` uses it. If it does not (a login with very many MCP server entries
can reach that), `cs` runs `security add-generic-password -U -s SERVICE -a ACCOUNT -w
JSON` with the JSON on the command line instead, and prints one warning on stderr that
the credentials were briefly visible to other local users in the process list, because
they exceeded the `security -i` line limit. The write still happens in full and is
never cut off. Both forms apply only to a macOS login that is in the Keychain and not
in a file. When `security` fails, the error includes the first 300 bytes of its
standard error (never the line that was sent).

`cs` writes each file with a same-directory temporary file, `fsync`, an atomic rename
and an `fsync` of the directory, so a crash or a power cut leaves the old file or the
new one, never half of it. If `~/.claude.json`, `~/.claude/.claude.json` or
`~/.claude/.credentials.json` is a symbolic link, a switch (`cs <alias>`, `cs next`,
rotation) replaces the link with a regular file; the file the link pointed to is left
as it was. This is how the original bash version behaves too. Keep them regular files,
or put the link back after a switch.

## An interrupted switch

A global switch changes up to three live files one after the other: the credentials
(the Keychain item on a Mac that keeps its login there, otherwise
`~/.claude/.credentials.json`), then `~/.claude.json`, then `~/.claude/.claude.json`.
No system call makes them one step, so a signal, a crash or a failed write between
them can leave the token of one account beside the display block of another.

Before the first of them changes, `cs` writes `~/.sharkswap/.identity.lock/switch-pending`
(mode 0600, the alias being switched to and nothing secret). It removes the file only
when every write has landed, or when every file has been put back after a failure. If
the run is stopped, or a write or a put-back fails, the file stays and so does the
identity lock directory it is in. This is the fail-closed state: nobody knows which
account the live login is.

While `switch-pending` exists:

- `cs status`, `cs` (the overview), `cs <alias>`, `cs next`, `cs best --switch`, `cs rotate`
  (also with `--dry-run`), `cs save`, `cs remove`, `cs pane`, the autosave of `cs watch`
  and `cs probe` all refuse. `cs probe` refuses before it sends a request. They print
  "unfinished account switch" and name the lock directory, at once and not after
  waiting for the lock to time out. (A switch that is still running is told apart
  below.)
- `cs list` still shows the saved accounts but marks none of them active, and `cs best`
  ranks them as before.
- `cs bar` with no alias prints `sharkswap: no account` instead of inferring one from
  the live files. `cs bar <alias>`, and `cs bar` run inside a pane (a `CLAUDE_CONFIG_DIR`
  under `~/.sharkswap/homes/`), are not about the live login and still work.
- A rotation whose own switch was cut short stops with an error. It does not record a
  switch or a failure, so the next run is not held back by a cooldown the switch never
  earned.

Only the existence of the file is checked. It is looked at with `lstat` and never
opened or read, so an empty file, a directory, a dangling link or a named pipe in its
place counts the same, and the status-line command `cs bar` cannot block on it.

### A switch that is still running

The file also exists for the few moments a healthy switch takes, while that `cs` holds
the lock. A command that meets it then must not send you to remove a lock that is in
use. So when the file exists, `cs` also reads the lock's `owner` file, a line
`pid=<N> started=<ISO time>`, and asks the system whether process N is alive (`kill(N, 0)`;
"not permitted" counts as alive):

- Alive: the switch is in progress. `cs status`, `cs`, `cs <alias>`, `cs next`,
  `cs toggle`, `cs best --switch`, `cs rotate` and `cs probe` refuse at once with "an
  account switch is in progress (owner: pid=N started=...); try again when it finishes"
  and say nothing about removing the lock. `cs save`, `cs remove`, `cs pane` and the
  autosave of `cs watch` take the identity lock, so they wait for it as they wait for any
  holder, and give "identity lock timed out" only after the usual 5 s.
- Gone, or no usable pid in `owner` (missing, empty, not `pid=<N>`, zero or negative):
  the switch was cut short. This is the "unfinished account switch" state and the rest
  of this section applies.

The owner file is read with the same bounded, non-blocking read as every file in the
store, and only when the marker exists. `cs bar` never reads it: it stays `lstat` only
and shows no inferred account while the marker exists, running or not. A process id can be
reused, so a holder that died may look alive for a while; that only turns the
"unfinished" refusal into "in progress" or a lock timeout, still a refusal, and the
lock is still never stolen.

There is no command that clears the state, on purpose: a person has to look first.

1. Stop whatever uses the global login: the rotation timer, `cs watch`, running
   `claude` sessions.
2. Look. `~/.sharkswap/.identity.lock/switch-pending` names the alias the switch was
   going to; `owner` names the process that held the lock. A switch that failed after
   it had changed something leaves copies of the previous files next to the live ones
   (`~/.claude.json.backup.*`, `~/.claude/.claude.json.backup.*`,
   `~/.claude/.credentials.json.switch-backup.*`, and on a Mac
   `~/.sharkswap/keychain-prior-login.*`); the switch's errors name each one ("a copy of
   the previous file is kept at ..."), including when only the marker could not be
   cleared. Keep them until you are sure which login you want.
3. Remove the abandoned lock: `rm -rf ~/.sharkswap/.identity.lock`.
4. Switch explicitly to an account you know is saved: `cs <alias>`. That rewrites all
   three files. If none is saved, run `/login` in Claude Code instead.
5. Check with `cs status`, then start the timer, `cs watch` and your sessions again.

Do not run `cs save` or `cs watch` to capture the login while it is uncertain: that
would store the mixed pair under an alias.

The guard covers interruptions that `cs` can see: a signal it handles, a write that
fails, a put-back that fails. It is not atomic against a power cut or a kill that comes
at the wrong instant, and it does not stop a program other than `cs` (Claude Code's own
`/login`) from writing the same files. A credentials write that fails also keeps the
marker, even when nothing was changed (a locked Keychain, say), because `cs` cannot
tell. Nothing recovers by itself.

## Usage probe

`cs probe` (and `cs`, `cs status`, `cs best` unless `QUICK=1`) sends one request with
`max_tokens: 1` per account to the Anthropic API and reads the
`anthropic-ratelimit-unified-5h-*` and `-7d-*` response headers. A reading is trusted
for 24 hours (`PROBE_MAX_AGE_HOURS` changes that). The request carries the system
prompt and beta header that Claude Code itself sends. It uses the model in
`SHARKSWAP_PROBE_MODEL` (default `claude-haiku-4-5-20251001`).

An answer of 200 means the account is usable. A rejected 5-hour or weekly window means
it is out, with the reset time. Anything else (network error, 5xx, an unexpected
status) is "unknown", never "out".

## Environment

| Variable | Effect |
|---|---|
| `QUICK=1` | `cs` and `cs status` skip the live probe. |
| `ASSUME_YES=1` | Do not ask for confirmation before a switch (rotation sets it for its own switch). |
| `PROBE_MAX_AGE_HOURS` | How long a probe reading is trusted (default 24). |
| `STALE_DAYS` | Age after which a saved login without a one-year token shows as `STALE` (default 7). |
| `WATCH_INTERVAL` | Seconds between checks in `cs watch` (default 10; 30 on macOS with a Keychain-only login, where each check runs `security`). |
| `SHARKSWAP_PROBE_MODEL` | Model for the probe request. |
| `SHARKSWAP_API_BASE` | Send probes to a different API address. Only loopback addresses (`http://127.0.0.1:PORT`, `localhost`, `[::1]`) are accepted, for testing against a stub. |
| `XDG_CONFIG_HOME`, `XDG_STATE_HOME` | Where the rotation pool, log and state go. |

## Performance

`watch` and `rotate` do their waiting inside the process. `watch` sleeps between checks
and, on Linux or with a credentials file, starts nothing per tick (on macOS with a
Keychain-only login it runs `security` once per tick); `rotate` runs once and exits.

Per-call wall time, 200 runs after a warm-up, with `hyperfine`, on a Linux desktop.
Eight saved accounts with fresh probe readings, `QUICK=1`, no network. The bash column
is the earlier bash and Node implementation this program replaces, given the same files.

| Command | Rust | bash + jq + node | Faster by |
|---|---|---|---|
| `cs status` | 1.2 ms | 68.6 ms | 55x |
| `cs list` | 1.7 ms | 71.6 ms | 43x |
| `cs bar` | 1.7 ms | 61.0 ms | 35x |
| `cs best` | 1.9 ms | 57.1 ms | 30x |

An idle `cs watch` over 60 seconds at the default 10 s interval (memory is the largest
sum of the process and its children; CPU time includes start-up):

| | Rust | bash + jq + node |
|---|---|---|
| Resident memory | 3.3 MB | 5.6 MB |
| CPU time | under 10 ms | 120 ms |

At a 1 s interval the Rust figure stays under 10 ms and the bash one is 150 ms.

`cs bar` is meant for a status line that refreshes every few seconds, which is why its
per-call cost matters.

## Tests

```sh
cargo test --locked
cargo test --locked --features test-hooks
bash test/run.sh
```

The first runs the Rust unit tests, the second the same with the hooks compiled in.
`bash test/run.sh` builds `target/test-hooks/` with the hooks, then runs the black-box
tests; `CS_BIN=/path/to/sharkswap bash test/run.sh` runs them against another build
that has the hooks.

`test/run.sh` runs the unit tests of the write-containment checker, the end-to-end
tests of the command line, and then the end-to-end tests again under `strace` to check
that every file the program creates, changes or deletes is inside a throwaway home
directory. Every test runs with a temporary `HOME`; the API is a stub on a loopback
port. Nothing reads your real `~/.claude`. The `strace` step needs Linux and permission
to ptrace; without it the suite falls back to checking that no file in the checkout
changed. Set `NO_TRACE=1` to skip that step.

The test scripts (not the program) need bash 4 or newer (macOS ships 3.2:
`brew install bash`), `jq`, `node` 20 or newer and `perl`; `strace` and `script` are
optional (`script` gives the one test that checks `token print` on a terminal a
pseudo-terminal; without it that test is skipped).

The tests never reach a real Keychain. The black-box suite runs a binary built with
the `test-hooks` cargo feature, tells it to call a stand-in `security` (a script in the
sandbox that logs its arguments and what it is fed, and uses made-up tokens), and can
make it take the macOS code path on Linux, so the Keychain code is exercised on every
system. The suite refuses to run against a binary without the hooks. In the unit
tests, the Keychain code panics unless a test hook points it at a stand-in or a
throwaway keychain.

The hooks are environment variables that exist only in a build made with
`--features test-hooks` (and in unit tests). A normal build, such as `cargo install`
or `cargo build --release` above, does not contain the code that reads them, so they
do nothing there; CI checks that the release binary does not contain the names at all.
Each is read only when `SHARKSWAP_TEST_HOOKS=1` is also set:

| Variable | Meaning |
|---|---|
| `SHARKSWAP_TEST_SECURITY` | Path of a program to run instead of `/usr/bin/security`. |
| `SHARKSWAP_TEST_DARWIN` | `1` forces the macOS code path (Keychain when there is no credentials file) on any system. |
| `SHARKSWAP_TEST_KEYCHAIN` | A keychain file to pass to `security` as its last argument. |
| `SHARKSWAP_TEST_SERVICE` | The item name to use instead of `Claude Code-credentials`. |
| `SHARKSWAP_NOW` | The instant, in epoch seconds, at which every availability judgement is made. |
| `SHARKSWAP_LOCK_ATTEMPTS` | How many times the identity lock is tried (default 50). |
| `SHARKSWAP_LOCK_DELAY` | Seconds between those tries (default 0.1). |
| `SHARKSWAP_TEST_SWITCH_FAULT` | Comma-separated `point:action`. Interrupts a switch at `publish`, `intent`, `creds`, `primary`, `secondary`, `rollback` or `clear`: `TERM` and `KILL` make the process signal itself there, `fail` makes that step report an error. |

The Keychain write is also tested against the real `security` tool, by two unit tests
that are ignored by default and run in the macOS CI job: `cargo test --locked --
--ignored keychain_round_trip keychain_line_limit_boundary`. The second writes a login
whose `security -i` line is exactly 4094 bytes (the most it takes on stdin) and one of
4095 bytes (which takes the command-line form) and reads each back byte for byte. They
use a throwaway keychain file that the job creates and deletes (`sharkswap-ci.keychain`,
or `sharkswap-ci.keychain-db`, which is what newer macOS versions create) and refuse
any other keychain. The hooks are for tests, not settings; do not set them.

`install.sh` refuses a binary that contains the hook names, so a build made with
`--features test-hooks` cannot be installed by mistake.

## Limitations

- It relies on Claude Code's current credential layout and on the rate-limit response
  headers. Neither is a documented, stable interface, and either can change.
- A saved login can go stale. OAuth refresh tokens rotate, so a snapshot of an account
  you keep using elsewhere may stop working. Re-save it, run `cs watch`, or use a
  one-year token.
- The probe spends one token of the account's allowance per request.
- The switch is global to the machine: it changes the login every new `claude` session
  uses. Use `cs pane` for per-terminal accounts.
- An interrupted switch is detected, not repaired: see "An interrupted switch" for what
  `cs` refuses until you have looked and cleared the lock yourself.
- Usage figures come from headers that can read a few points above the usage page.
- No Windows support.

## License

[PolyForm Noncommercial License 1.0.0](LICENSE). You may use, change and share
this software for any noncommercial purpose: personal use, hobby projects, study,
research, and use by charities, schools and other noncommercial organizations.
Any commercial use needs a separate license from the copyright holder; ask
[spicysharkco](https://github.com/spicysharkco) on GitHub.

Contributions are welcome. Each one is assigned to spicysharkco under the
contributor agreement in [CONTRIBUTING.md](CONTRIBUTING.md), so spicysharkco owns
the whole project.

To report a security problem, see [SECURITY.md](SECURITY.md).
