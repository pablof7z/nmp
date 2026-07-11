#!/usr/bin/env bash
set -euo pipefail

ROOT=${SURFACE_ROOT:-$(git rev-parse --show-toplevel)}
# shellcheck disable=SC1091
source "${SURFACE_TOOLCHAIN_ENV:-$ROOT/tools/surface-toolchain.env}"

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
[[ -n "$crate" ]] || { echo "cargo-public-api crate archive not found" >&2; exit 1; }
if command -v sha256sum >/dev/null; then
  actual=$(sha256sum "$crate" | awk '{print $1}')
else
  actual=$(shasum -a 256 "$crate" | awk '{print $1}')
fi
[[ $actual == "$CARGO_PUBLIC_API_CRATE_SHA256" ]] || {
  echo "cargo-public-api crate checksum mismatch: $actual" >&2
  exit 1
}
