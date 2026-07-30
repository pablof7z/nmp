#!/usr/bin/env bash
# Mutation falsifiers for scripts/check-rust-toolchain.sh.

set -euo pipefail

ROOT=$(git rev-parse --show-toplevel)
CHECKER="$ROOT/scripts/check-rust-toolchain.sh"
BASH_BIN=$(command -v bash)
TEMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/nmp-rust-toolchain.XXXXXX")
FIXTURE_ROOT="$TEMP_ROOT/repo"
FAKE_PATH="$TEMP_ROOT/bin"
trap 'rm -rf "$TEMP_ROOT"' EXIT

fail() {
  echo "rust-toolchain test: $*" >&2
  exit 1
}

# shellcheck disable=SC1091
source "$ROOT/tools/rust-toolchain-versions.env"
channel=$(
  sed -nE \
    's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*"([^"]+)".*$/\1/p' \
    "$ROOT/rust-toolchain.toml"
)

mkdir -p "$FAKE_PATH"
ln -s "$(command -v grep)" "$FAKE_PATH/grep"
ln -s "$(command -v sed)" "$FAKE_PATH/sed"

cat > "$FAKE_PATH/rustup" <<'SH'
#!/bin/sh
if [ "$1" = "show" ] && [ "$2" = "active-toolchain" ]; then
  printf '%s (overridden by fixture)\n' "$FAKE_ACTIVE_TOOLCHAIN"
  exit 0
fi
exit 64
SH

cat > "$FAKE_PATH/rustc" <<'SH'
#!/bin/sh
if [ "$1" = "--version" ]; then
  printf '%s\n' "$FAKE_RUSTC_VERSION"
  exit 0
fi
exit 64
SH

cat > "$FAKE_PATH/cargo" <<'SH'
#!/bin/sh
case "$1 $2" in
  "--version ")
    printf '%s\n' "$FAKE_CARGO_VERSION"
    ;;
  "clippy --version")
    printf '%s\n' "$FAKE_CLIPPY_VERSION"
    ;;
  "fmt --version")
    printf '%s\n' "$FAKE_RUSTFMT_VERSION"
    ;;
  *)
    exit 64
    ;;
esac
SH
chmod +x "$FAKE_PATH/rustup" "$FAKE_PATH/rustc" "$FAKE_PATH/cargo"

reset_fixture() {
  rm -rf "$FIXTURE_ROOT"
  mkdir -p "$FIXTURE_ROOT/tools"
  cp "$ROOT/rust-toolchain.toml" "$FIXTURE_ROOT/"
  cp "$ROOT/tools/surface-toolchain.env" "$FIXTURE_ROOT/tools/"
  cp "$ROOT/tools/rust-toolchain-versions.env" "$FIXTURE_ROOT/tools/"
}

run_checker() {
  local active=${1:-"$channel-x86_64-unknown-linux-gnu"}
  PATH="$FAKE_PATH" \
    FAKE_ACTIVE_TOOLCHAIN="$active" \
    FAKE_RUSTC_VERSION="$RUSTC_VERSION" \
    FAKE_CARGO_VERSION="$CARGO_VERSION" \
    FAKE_CLIPPY_VERSION="$CLIPPY_VERSION" \
    FAKE_RUSTFMT_VERSION="$RUSTFMT_VERSION" \
    "$BASH_BIN" "$CHECKER" "$FIXTURE_ROOT"
}

expect_failure() {
  local label=$1
  local expected=$2
  shift 2
  local output
  if output=$(run_checker "$@" 2>&1); then
    fail "$label mutation unexpectedly passed"
  fi
  grep -Fq -- "$expected" <<< "$output" ||
    fail "$label mutation failed for the wrong reason: $output"
}

reset_fixture
run_checker >/dev/null

reset_fixture
sed -i.bak 's/^channel = .*/channel = "nightly"/' \
  "$FIXTURE_ROOT/rust-toolchain.toml"
rm "$FIXTURE_ROOT/rust-toolchain.toml.bak"
expect_failure "floating nightly" "channel must be an exact dated nightly"

reset_fixture
sed -i.bak 's/^channel = .*/channel = "nightly-2099-01-01"/' \
  "$FIXTURE_ROOT/rust-toolchain.toml"
rm "$FIXTURE_ROOT/rust-toolchain.toml.bak"
expect_failure "root/surface mismatch" \
  "root pin nightly-2099-01-01 does not match surface pin $channel"

reset_fixture
expect_failure "active-toolchain mismatch" \
  "active toolchain nightly-2099-01-01-x86_64-unknown-linux-gnu" \
  "nightly-2099-01-01-x86_64-unknown-linux-gnu"

reset_fixture
sed -i.bak \
  "s/^RUSTC_VERSION=.*/RUSTC_VERSION='rustc mismatched'/" \
  "$FIXTURE_ROOT/tools/rust-toolchain-versions.env"
rm "$FIXTURE_ROOT/tools/rust-toolchain-versions.env.bak"
expect_failure "version mismatch" "rustc mismatch"

reset_fixture
sed -i.bak 's/^components = .*/components = ["rustfmt"]/' \
  "$FIXTURE_ROOT/rust-toolchain.toml"
rm "$FIXTURE_ROOT/rust-toolchain.toml.bak"
expect_failure "missing clippy" "must install clippy"

echo "rust-toolchain test: baseline and five mutations passed"
