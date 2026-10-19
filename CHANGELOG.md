# Changelog

All notable changes to sharkswap are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). Until 1.0, a minor
version (0.x.0) may change commands, output or file formats.

## [Unreleased]

### Changed

- `cs best` and `cs probe` show when the 5-hour window resets, not only the week. A row
  whose 5-hour window is spent (`BLOCKED` in `best`, `5H FULL` in `probe`) used to show only
  the week reset, such as `in 1d12h`, when it reopens in under an hour. The `best` table
  replaces its `RESETS AT` ISO column with `WK-RESET` and `5H-RESET`, written as
  `in 5d08h` from a day on and `in 0h42m` below it, or `-` when the reset is unknown or has
  passed. The `probe` table keeps `WEEK-RESET` and gains `5H-RESET` after it, in the
  same form; under a day, `WEEK-RESET` now reads `in 23h59m` where it read `in 0d23h`.
- `cs best --raw` appends a 7th field, the time the 5-hour window resets (ISO time, or `-`),
  after the 5-hour %. Fields 1 to 6 keep their places, so a script that picks fields by
  number keeps working; one that reads exactly six fields, such as `read -r a b c d e f`,
  now gets the 5-hour % and the new field together in its last variable.

## [0.4.0] - 2026-10-18

### Changed

- `cs probe` (and `cs`, `cs status`) print, below the table, one line on stderr for each
  account the API answered with 401 or 403, with the message from the error body (printable
  ASCII, cut to 120 characters; the body is read up to 8 KiB, never a header or the
  token). No line is printed for a body over 8 KiB, one that is not JSON or is empty, or
  a message that holds the token or any 24 consecutive characters of it. For the 403
  "OAuth authentication is currently not allowed for this organization." a second line
  says the organization does not allow this login now and to check the account's
  subscription or billing. The table cell is still `AUTH 403`, and `best`, `bar` and the
  `--raw` output are unchanged.
- `cs token set <alias>` compares the organization the API places the new token in (the
  `anthropic-organization-id` header of one one-token request, which can take up to 30
  seconds) with the `organizationUuid` of the alias's saved login. A token from another
  organization, such as one made while another account was logged in, is refused with
  exit 75, both ids named, the other possible cause (the account's organization changed
  since `cs save`) named and nothing stored; `cs token set --force <alias>` stores it
  anyway. When the two cannot be compared, the token is stored as before with a note: the
  saved login names no organization id or one that is not a UUID, the API did not answer,
  or its reply has no valid organization id (a 401, a 403 or another error status say so
  in their own note; the 401 and 403 notes carry the message from the error body).
  Accounts that share one organization cannot be told apart this way.

### Fixed

- `cs rotate` no longer overwrites a login you switched to by hand while it was
  probing. Rotation now tells its own switch the email of the login the decision was made
  from (`SHARKSWAP_EXPECT_FROM`), and that switch checks the active login again once it
  holds the identity lock, so a login that is not a saved account is covered too. If it
  moved, nothing is changed, the switch prints `error: the active login is '<now>', not
  '<expected>'; nothing was changed (set by SHARKSWAP_EXPECT_FROM)` and exits 76, and
  rotation logs that it did not run, with no warm-up request, no recorded failure and no
  switch time. The next run decides again. The check is skipped when rotation found no
  email (no login, or `.claude.json` unreadable or without an email at that moment). The
  lock stops only other sharkswap switches, not a login or token refresh by another
  program, so the check narrows the window rather than closing it.

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

[Unreleased]: https://github.com/spicysharkco/sharkswap/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/spicysharkco/sharkswap/releases/tag/v0.4.0
[0.3.0]: https://github.com/spicysharkco/sharkswap/releases/tag/v0.3.0
[0.2.0]: https://github.com/spicysharkco/sharkswap/releases/tag/v0.2.0
[0.1.0]: https://github.com/spicysharkco/sharkswap/tree/f0edcbb2040824a4d541f104e0475f845c914bbe
