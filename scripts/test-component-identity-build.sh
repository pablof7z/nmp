#!/usr/bin/env bash
# #952 release-boundary falsifier. A native component release must not mint an
# identity outside the supported builders, because only those builders select
# the exact core-only or matched core/provider Cargo resolution. Cargo exposes
# built-in `bench` to build scripts as release-class, so probe both spellings.

set -euo pipefail

ROOT=$(git rev-parse --show-toplevel)
cd "$ROOT"

TMP=$(mktemp -d "${TMPDIR:-/tmp}/nmp-component-build.XXXXXX")
trap 'rm -rf "$TMP"' EXIT

assert_unmanaged_refused() {
  local label=$1
  shift
  local output="$TMP/$label-output"

  if env -u NMP_FFI_COMPONENT_BUILD \
    cargo build --locked -p nmp-ffi "$@" >"$output" 2>&1; then
    echo "component-identity-build: unmanaged $label build unexpectedly succeeded" >&2
    exit 1
  fi

  grep -qF \
    "release native components must use the supported Swift or Kotlin builder" \
    "$output" || {
      cat "$output" >&2
      echo "component-identity-build: $label failed for the wrong reason" >&2
      exit 1
    }
}

assert_unmanaged_refused release --release
assert_unmanaged_refused bench --profile bench

echo "component-identity-build: unmanaged release-class builds refused"
