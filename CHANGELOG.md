# Changelog

All notable changes to sharkswap are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.0.0] - 2026-10-13

First public release. sharkswap is a single Rust binary (also run as `cs`) that
switches between several Claude Code accounts without logging in again.

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
- Prebuilt binaries on the releases page for macOS (Apple silicon and Intel) and Linux
  (x86-64 and ARM64, statically linked), with a `SHA256SUMS` file and build provenance
  attestations. Each archive carries `install.sh`, which installs the binary and the `cs`
  link (`./install.sh --binary ./sharkswap`), and `cargo binstall --git` can use the
  archives too. `cargo install --locked --git` still builds from source.

[1.0.0]: https://github.com/spicysharkco/sharkswap/releases/tag/v1.0.0
