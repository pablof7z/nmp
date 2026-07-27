#!/usr/bin/env bash
# #952 release-boundary falsifier. A native component release must not mint an
# identity outside the isolated supported builders, because only those builders
# fix the Cargo roots and target directory before the build starts.

set -euo pipefail

ROOT=$(git rev-parse --show-toplevel)
cd "$ROOT"

TMP=$(mktemp -d "${TMPDIR:-/tmp}/nmp-component-build.XXXXXX")
trap 'rm -rf "$TMP"' EXIT

TARGET_DIR_VALUE=${CARGO_TARGET_DIR:-target}
if [[ "$TARGET_DIR_VALUE" == /* ]]; then
  BASE_TARGET_DIR=$TARGET_DIR_VALUE
else
  BASE_TARGET_DIR="$ROOT/$TARGET_DIR_VALUE"
fi
HOST_TARGET=$(rustc -vV | sed -n 's/^host: //p')
[[ -n "$HOST_TARGET" ]] || {
  echo "component-identity-build: rustc did not report a host target" >&2
  exit 1
}
CORE_BUILD=$(
  scripts/prepare-component-build.sh "$BASE_TARGET_DIR" "nmp-ffi" "$HOST_TARGET"
)
CORE_TARGET_DIR=${CORE_BUILD%%$'\n'*}
PAIR_BUILD=$(
  scripts/prepare-component-build.sh \
    "$BASE_TARGET_DIR" "nmp-ffi nmp-nip46-ffi" "$HOST_TARGET"
)
PAIR_TARGET_DIR=${PAIR_BUILD%%$'\n'*}

assert_unmanaged_refused() {
  local label=$1
  local target_dir=$2
  shift 2
  local output="$TMP/$label-output"

  if env -u NMP_FFI_COMPONENT_AUTH \
    CARGO_TARGET_DIR="$target_dir" \
    "$@" >"$output" 2>&1; then
    echo "component-identity-build: unmanaged $label build unexpectedly succeeded" >&2
    exit 1
  fi

  grep -qF \
    "release component authorization does not match its isolated target" \
    "$output" || {
      cat "$output" >&2
      echo "component-identity-build: $label failed for the wrong reason" >&2
      exit 1
    }
}

assert_unmanaged_refused core "$CORE_TARGET_DIR" \
  cargo build --locked -p nmp-ffi --release
assert_unmanaged_refused pair "$PAIR_TARGET_DIR" \
  cargo build --locked -p nmp-ffi -p nmp-nip46-ffi --release
assert_unmanaged_refused workspace "$PAIR_TARGET_DIR" \
  cargo build --locked --workspace --release
assert_unmanaged_refused all-targets "$CORE_TARGET_DIR" \
  cargo build --locked -p nmp-ffi --all-targets --release
assert_unmanaged_refused test "$CORE_TARGET_DIR" \
  cargo test --locked -p nmp-ffi --release --no-run
assert_unmanaged_refused clippy "$CORE_TARGET_DIR" \
  cargo clippy --locked -p nmp-ffi --release --no-deps
assert_unmanaged_refused bench "$CORE_TARGET_DIR" \
  cargo build --locked -p nmp-ffi --profile bench

echo "component-identity-build: every unauthorized release-class shape refused inside packageable targets"
