# Changelog

All notable changes to sharkswap are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Until 1.0, a minor
version (0.x.0) may change commands, output or file formats.

## [0.3.0] - 2026-10-17

### Added

- `--version` and `-V` print the version (`sharkswap 0.3.0`) and exit. Both words are
  reserved, so `save` refuses them as alias names, and an existing alias spelled like
  either word can still be removed but not switched to.
- `SECURITY.md`: report a vulnerability through GitHub private vulnerability reporting.

### Changed

- `help`, `-h`, `--help`, `--version` and `-V` no longer create `~/.sharkswap` or change
  its mode: they print their text and exit.
- The release smoke test checks that the unpacked binary prints the version being released.

## [0.2.0] - 2026-10-16

### Added

- Prebuilt binaries on the releases page for macOS (Apple silicon and Intel) and Linux
  (x86-64 and ARM64, statically linked), with a `SHA256SUMS` file and build provenance
  attestations. Each archive carries `install.sh`, which installs the binary and the `cs`
  link (`./install.sh --binary ./sharkswap`), and `cargo binstall --git` can use the
  archives too. `cargo install --locked --git` still builds from source.
- An interrupted switch is detected. Before the first live file changes, a global switch
  writes `~/.sharkswap/.identity.lock/switch-pending`, and it removes the file only when
  every write landed or every file was put back. While the file is there, the commands
  that read or change the live login refuse with "unfinished account switch" instead of
  reading a token of one account beside the display block of another. See "An
  interrupted switch" in the README.
- A switch that is still running is told apart from one that was cut short, by the pid in
  the lock's `owner` file. Commands refuse with "an account switch is in progress" or wait
  for the lock, and do not tell you to remove a lock that is in use.

### Changed

- `cs rotate` logs a note once, as `note: <text>` when it first appears and
  `note ended: <text>` when it goes away, and no longer on every scheduled run. stderr
  still prints every note on every run.

### Fixed

- A store, cache, state, credentials or config file that is a named pipe no longer
  blocks a read forever. Each such file is opened without blocking, checked to be a
  regular file within a size cap, and read through that cap; anything else counts as a
  corrupt file.
- `cs save` reads the live login under the identity lock, so a switch that is running at
  the same time can no longer slip a capped login past the exit-75 refusal under a second
  name.
- A failed switch names each copy of a previous file it kept and the credentials file a
  write failed on, and several failures that printed nothing now say what failed and why.
- Owner text from the lock is shown as one line of printable ASCII, cut at 120
  characters.

### Security

- rustls 0.23.46, which fixes RUSTSEC-2026-0346, RUSTSEC-2026-0347, RUSTSEC-2026-0348
  and RUSTSEC-2026-0349.

## [0.1.0] - 2026-10-13

First public release, as a commit with no tag or release. sharkswap is a single
Rust binary (also run as `cs`) that switches between several Claude Code accounts
without logging in again. That commit's `Cargo.toml` says 1.0.0; the project is
numbered as a beta from 0.2.0 on, and this entry carries the number it should have had.

### Added

- Saved accounts. `cs save ALIAS` snapshots the current login (the OAuth token and
  the display block) under a name; `cs list` shows them and `cs remove ALIAS` deletes
  one. Files live under `~/.sharkswap/` with mode 0600 and are written atomically.
- Switching. `cs ALIAS` makes a saved account the live login, `cs next` goes round
  robin, and `cs pane ALIAS` prints the settings for a second terminal that uses
  another account at the same time, in its own config directory.
- Usage. `cs`, `cs status` and `cs probe` send one 1-token request per account and read
  the 5-hour and weekly use from the rate-limit response headers. `cs best` ranks the
  accounts by remaining room and can switch to the best one (`--switch`); `cs bar`
  prints one status-bar line.
- Ceilings. `~/.sharkswap/account-caps.json` sets a percent limit per account for the
  5-hour window, the weekly window or both. A switch to an account at its ceiling is
  refused with exit 75 unless `--force` is given.
- Rotation. `cs rotate` does one run for a cron, systemd or launchd timer: it stays on an
  account until the provider refuses it or it reaches its ceiling, then moves to the
  account in the pool with the most room. It has a minimum gap between switches, a
  dry run (`--dry-run`), a log, and a lock so two runs never overlap.
- One-year tokens. `cs token set|rm|list|check|print` manage tokens from
  `claude setup-token`, which need no monthly re-login. `cs pane` hands such a token to
  the new shell without printing it.
- `cs watch`, which re-saves the active token whenever Claude Code refreshes it.
- macOS Keychain support through `/usr/bin/security`, and file-based logins on Linux.

[0.3.0]: https://github.com/spicysharkco/sharkswap/releases/tag/v0.3.0
[0.2.0]: https://github.com/spicysharkco/sharkswap/releases/tag/v0.2.0
[0.1.0]: https://github.com/spicysharkco/sharkswap/tree/f0edcbb2040824a4d541f104e0475f845c914bbe
