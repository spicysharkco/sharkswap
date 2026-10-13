#!/usr/bin/env bash
# Install sharkswap from a checkout into a bin directory.
#
#   ./install.sh                 builds (if needed) and installs into ~/.local/bin
#   ./install.sh --prefix DIR    installs into DIR
#   ./install.sh --binary FILE   installs this built binary instead of building one
#   ./install.sh --uninstall     removes what this script installed
#
# The binary is copied to DIR/sharkswap, and `cs` is made a link to it. A file
# of either name that is not this tool is never replaced or removed. With cargo,
# `cargo install --locked --git https://github.com/spicysharkco/sharkswap` does the first
# half without this script.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
bin="${HOME}/.local/bin"
src=""
uninstall=0

while (( $# )); do
  case "$1" in
    --prefix) (( $# >= 2 )) || { echo "install: --prefix needs a directory" >&2; exit 2; }; bin="$2"; shift 2 ;;
    --binary) (( $# >= 2 )) || { echo "install: --binary needs a file" >&2; exit 2; }; src="$2"; shift 2 ;;
    --uninstall) uninstall=1; shift ;;
    -h|--help) awk 'NR > 1 { if ($0 !~ /^#/) exit; sub(/^# ?/, ""); print }' "$0"; exit 0 ;;
    *) echo "install: unknown argument '$1'" >&2; exit 2 ;;
  esac
done

# Whether FILE is a sharkswap program: it answers `help` with this tool's
# first line. Anything else is somebody else's and is left alone.
is_ours() {
  [[ -f "$1" && -x "$1" && ! -L "$1" ]] || return 1
  [[ "$("$1" help 2>/dev/null | head -n 1 || true)" == "sharkswap: a Claude Code account switcher." ]]
}

# Copy SRC over DST through a temporary file in the same directory, so a running
# copy is never overwritten in place.
put() {
  local tmp
  tmp="$(mktemp "$bin/.sharkswap.XXXXXX")"
  if cp "$1" "$tmp" && chmod 755 "$tmp" && mv -f "$tmp" "$2"; then return 0; fi
  rm -f "$tmp"
  return 1
}

if (( uninstall )); then
  if is_ours "$bin/sharkswap"; then
    rm -f "$bin/sharkswap" && echo "removed $bin/sharkswap"
  fi
  if [[ -L "$bin/cs" && "$(readlink "$bin/cs")" == sharkswap ]]; then
    rm -f "$bin/cs" && echo "removed $bin/cs"
  fi
  exit 0
fi

if [[ -z "$src" ]]; then
  src="$here/target/release/sharkswap"
  if [[ ! -x "$src" ]]; then
    command -v cargo >/dev/null 2>&1 || { echo "install: cargo not found; install Rust (https://rustup.rs) or pass --binary FILE" >&2; exit 1; }
    (cd "$here" && cargo build --release --locked)
  fi
fi
is_ours "$src" || { echo "install: $src is not a sharkswap binary" >&2; exit 1; }

# A build with the test hooks (cargo feature test-hooks) reads settings from the
# environment that an installed tool must ignore, and on a Mac it can be pointed
# at another program in place of `security`. It is for the tests only: refuse it.
hooked=0
LC_ALL=C grep -aq SHARKSWAP_TEST_SECURITY "$src" || hooked=$?
case "$hooked" in
  0) echo "install: $src was built with the test hooks (feature test-hooks); build it without that feature: cargo build --release --locked" >&2; exit 1 ;;
  1) ;;
  *) echo "install: cannot read $src" >&2; exit 1 ;;
esac

# HOME's .local/bin is the usual place and may not exist yet.
mkdir -p "$bin"
if [[ -e "$bin/sharkswap" || -L "$bin/sharkswap" ]]; then
  if is_ours "$bin/sharkswap" && cmp -s "$src" "$bin/sharkswap"; then
    echo "ok      $bin/sharkswap (already installed)"
  elif is_ours "$bin/sharkswap"; then
    put "$src" "$bin/sharkswap"
    echo "updated $bin/sharkswap"
  else
    echo "skipped $bin/sharkswap exists and is not sharkswap; remove it first" >&2
  fi
else
  put "$src" "$bin/sharkswap"
  echo "installed $bin/sharkswap"
fi

if [[ -e "$bin/cs" || -L "$bin/cs" ]]; then
  if [[ -L "$bin/cs" && "$(readlink "$bin/cs")" == sharkswap ]]; then
    echo "ok      $bin/cs (already linked)"
  else
    echo "skipped $bin/cs exists and is not a link to sharkswap; remove it first" >&2
  fi
else
  ln -s sharkswap "$bin/cs"
  echo "linked  $bin/cs -> sharkswap"
fi

case ":$PATH:" in
  *":$bin:"*) ;;
  *) echo "note: $bin is not on your PATH; add it to your shell profile" ;;
esac
