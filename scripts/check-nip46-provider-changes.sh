#!/usr/bin/env bash
# Decide whether the expensive Ubuntu NIP-46 package proofs can be affected by
# a pull request. Classification failure is intentionally fail-closed: run the
# proofs instead of silently weakening the boundary.

set -euo pipefail

SCRIPT_PATH=${BASH_SOURCE[0]}
SCRIPT_DIR=${SCRIPT_PATH%/*}
[[ $SCRIPT_DIR != "$SCRIPT_PATH" ]] || SCRIPT_DIR=.
source "$SCRIPT_DIR/lib/require-commands.sh" || exit 2
require_commands git mktemp rm || exit 2

if [[ $# -ne 2 ]]; then
  echo "usage: $0 BASE_REF HEAD_REF" >&2
  exit 2
fi

BASE_REF=$1
HEAD_REF=$2
ROOT=${NMP_ROOT:-$(git rev-parse --show-toplevel)}
CHANGED_PATHS=$(mktemp "${TMPDIR:-/tmp}/nmp-nip46-provider-changes.XXXXXX")
trap 'rm -f "$CHANGED_PATHS"' EXIT

if ! git -C "$ROOT" diff --name-only -z "$BASE_REF...$HEAD_REF" > "$CHANGED_PATHS"; then
  echo "nip46-provider-changes: diff unavailable; running proofs" >&2
  printf 'true\n'
  exit 0
fi

while IFS= read -r -d '' path; do
  case "$path" in
    .github/workflows/nip46-provider.yml|\
    Cargo.toml|\
    Cargo.lock|\
    rust-toolchain.toml|\
    crates/*|\
    Packages/NMPKotlin/*|\
    scripts/build-component-release.sh|\
    scripts/build-kotlin-jvm.sh|\
    scripts/build-kotlin-nip46-jvm.sh|\
    scripts/check-nip46-*|\
    scripts/test-component-identity-build.sh|\
    scripts/test-nip46-*|\
    scripts/lib/*)
      printf 'true\n'
      exit 0
      ;;
  esac
done < "$CHANGED_PATHS"

printf 'false\n'
