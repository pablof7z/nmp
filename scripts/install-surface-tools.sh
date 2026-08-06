#!/usr/bin/env bash
set -euo pipefail

# Nothing this program installs is a statement about a proposed head, so every
# way it can fail is a gate malfunction and it exits 70 for all of them, the
# same code check-surface-governance.sh uses (#1264). Without this a `git`,
# `rustup` or `cargo` failure escaped as its own raw status -- exit 128 for a
# fatal git, which is neither a verdict nor legibly a malfunction, and which no
# reporter could classify because the process it would have reported on never
# ran.
MALFUNCTION_EXIT=70
malfunction() {
  echo "surface-tools-malfunction: $*" >&2
  exit "$MALFUNCTION_EXIT"
}
trap 'malfunction "the surface tool install did not complete: line $LINENO exited $?"' ERR

# This program takes nothing from its caller -- not an environment variable and
# not a working directory. The toolchain definition is sourced, so it runs, and
# it is this program's own copy rather than the tree the gate is judging
# (#1186); in CI that is the scratch directory extracted from the base commit.
# Every toolchain below is then named explicitly, so no `rust-toolchain.toml`
# in any directory decides what runs.
PROGRAM_ROOT=$(cd "$(dirname "$0")/.." && pwd)
[[ -f "$PROGRAM_ROOT/tools/surface-toolchain.env" ]] ||
  malfunction "this program has no toolchain definition: $PROGRAM_ROOT/tools/surface-toolchain.env"
# shellcheck disable=SC1091
source "$PROGRAM_ROOT/tools/surface-toolchain.env"

rustup toolchain install "$SURFACE_RUST_TOOLCHAIN" --profile minimal

if ! command -v cargo-public-api >/dev/null ||
   [[ $(cargo-public-api --version) != "cargo-public-api $CARGO_PUBLIC_API_VERSION" ]]; then
  cargo "+$SURFACE_RUST_TOOLCHAIN" install --locked "cargo-public-api@$CARGO_PUBLIC_API_VERSION"
fi

CACHE_ROOT=${CARGO_HOME:-$HOME/.cargo}/registry/cache
crate=""
if [[ -d "$CACHE_ROOT" ]]; then
  crate=$(find "$CACHE_ROOT" -name "cargo-public-api-$CARGO_PUBLIC_API_VERSION.crate" -print -quit)
fi
if [[ -z "$crate" ]]; then
  # A preinstalled binary with a pruned Cargo cache is not evidence for the
  # package checksum. Fetch the exact locked package again so it can be
  # verified before use.
  cargo "+$SURFACE_RUST_TOOLCHAIN" install --locked --force \
    "cargo-public-api@$CARGO_PUBLIC_API_VERSION"
  if [[ -d "$CACHE_ROOT" ]]; then
    crate=$(find "$CACHE_ROOT" -name "cargo-public-api-$CARGO_PUBLIC_API_VERSION.crate" -print -quit)
  fi
fi
[[ -n "$crate" ]] || malfunction "cargo-public-api crate archive not found"
if command -v sha256sum >/dev/null; then
  actual=$(sha256sum "$crate" | awk '{print $1}')
else
  actual=$(shasum -a 256 "$crate" | awk '{print $1}')
fi
[[ $actual == "$CARGO_PUBLIC_API_CRATE_SHA256" ]] ||
  malfunction "cargo-public-api crate checksum mismatch: $actual"
