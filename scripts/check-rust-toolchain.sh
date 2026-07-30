#!/usr/bin/env bash
# #916: local and CI builds must select one exact compiler, Cargo, Clippy, and
# rustfmt. A floating nightly or a root/surface/version mismatch fails closed.

set -euo pipefail

SCRIPT_PATH=${BASH_SOURCE[0]}
SCRIPT_DIR=${SCRIPT_PATH%/*}
[[ $SCRIPT_DIR != "$SCRIPT_PATH" ]] || SCRIPT_DIR=.
source "$SCRIPT_DIR/lib/require-commands.sh" || exit 2
require_commands cargo grep rustc rustup sed || exit 2

if [[ -n ${1:-} ]]; then
  [[ $# -eq 1 ]] || {
    echo "rust-toolchain contract: usage: $0 [repo-root]" >&2
    exit 2
  }
  ROOT=$1
else
  require_commands git || exit 2
  ROOT=$(git rev-parse --show-toplevel)
fi

TOOLCHAIN_FILE="$ROOT/rust-toolchain.toml"
SURFACE_ENV="$ROOT/tools/surface-toolchain.env"
VERSION_ENV="$ROOT/tools/rust-toolchain-versions.env"

fail() {
  echo "rust-toolchain contract: $*" >&2
  exit 1
}

for required_file in "$TOOLCHAIN_FILE" "$SURFACE_ENV" "$VERSION_ENV"; do
  [[ -f "$required_file" ]] ||
    fail "missing ${required_file#"$ROOT/"}"
done

channel=$(
  sed -nE \
    's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*"([^"]+)"[[:space:]]*(#.*)?$/\1/p' \
    "$TOOLCHAIN_FILE"
)
[[ -n $channel && $channel != *$'\n'* ]] ||
  fail "rust-toolchain.toml must declare exactly one string channel"
[[ $channel =~ ^nightly-[0-9]{4}-[0-9]{2}-[0-9]{2}$ ]] ||
  fail "channel must be an exact dated nightly, got: $channel"

components=$(
  sed -nE \
    's/^[[:space:]]*components[[:space:]]*=[[:space:]]*\[(.*)\][[:space:]]*(#.*)?$/\1/p' \
    "$TOOLCHAIN_FILE"
)
[[ -n $components && $components != *$'\n'* ]] ||
  fail "rust-toolchain.toml must declare exactly one components array"
[[ $components == *'"rustfmt"'* ]] ||
  fail "rust-toolchain.toml must install rustfmt"
[[ $components == *'"clippy"'* ]] ||
  fail "rust-toolchain.toml must install clippy"

surface_channel=$(
  sed -nE 's/^SURFACE_RUST_TOOLCHAIN=([^[:space:]]+)$/\1/p' "$SURFACE_ENV"
)
[[ -n $surface_channel && $surface_channel != *$'\n'* ]] ||
  fail "tools/surface-toolchain.env must declare one SURFACE_RUST_TOOLCHAIN"
[[ $channel == "$surface_channel" ]] ||
  fail "root pin $channel does not match surface pin $surface_channel"

# shellcheck disable=SC1090
source "$VERSION_ENV"
: "${RUSTC_VERSION:?missing RUSTC_VERSION in $VERSION_ENV}"
: "${CARGO_VERSION:?missing CARGO_VERSION in $VERSION_ENV}"
: "${CLIPPY_VERSION:?missing CLIPPY_VERSION in $VERSION_ENV}"
: "${RUSTFMT_VERSION:?missing RUSTFMT_VERSION in $VERSION_ENV}"

pushd "$ROOT" >/dev/null
active_line=$(rustup show active-toolchain)
active_toolchain=${active_line%% *}
[[ $active_toolchain == "$channel"-* ]] ||
  fail "active toolchain $active_toolchain does not match repository pin $channel"

actual_rustc=$(rustc --version)
actual_cargo=$(cargo --version)
actual_clippy=$(cargo clippy --version)
actual_rustfmt=$(cargo fmt --version)
popd >/dev/null

[[ $actual_rustc == "$RUSTC_VERSION" ]] ||
  fail "rustc mismatch: expected '$RUSTC_VERSION', got '$actual_rustc'"
[[ $actual_cargo == "$CARGO_VERSION" ]] ||
  fail "cargo mismatch: expected '$CARGO_VERSION', got '$actual_cargo'"
[[ $actual_clippy == "$CLIPPY_VERSION" ]] ||
  fail "clippy mismatch: expected '$CLIPPY_VERSION', got '$actual_clippy'"
[[ $actual_rustfmt == "$RUSTFMT_VERSION" ]] ||
  fail "rustfmt mismatch: expected '$RUSTFMT_VERSION', got '$actual_rustfmt'"

printf 'rust-toolchain: %s\n%s\n%s\n%s\n%s\n' \
  "$channel" \
  "$actual_rustc" \
  "$actual_cargo" \
  "$actual_clippy" \
  "$actual_rustfmt"
