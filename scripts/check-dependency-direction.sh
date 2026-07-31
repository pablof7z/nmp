#!/usr/bin/env bash
# #922: enforce protocol/generic direction from Cargo's resolved all-features
# normal/build graph, not from opt-in package registration.

set -euo pipefail

SCRIPT_PATH=${BASH_SOURCE[0]}
SCRIPT_DIR=${SCRIPT_PATH%/*}
[[ $SCRIPT_DIR != "$SCRIPT_PATH" ]] || SCRIPT_DIR=.
source "$SCRIPT_DIR/lib/require-commands.sh" || exit 2
require_commands cargo python3 mktemp rm || exit 2

ROOT=$(cd "$SCRIPT_DIR/.." && pwd)
POLICY="$ROOT/scripts/dependency-direction-policy.json"
VALIDATOR="$ROOT/scripts/check-dependency-direction.py"
USE_LOCKED=true

if [[ ${1:-} == "--unlocked" ]]; then
  USE_LOCKED=false
  shift
fi
if [[ $# -gt 1 ]]; then
  echo "dependency-direction: usage: $0 [--unlocked] [Cargo.toml]" >&2
  exit 2
fi

MANIFEST=${1:-"$ROOT/Cargo.toml"}
METADATA=$(mktemp "${TMPDIR:-/tmp}/nmp-dependency-direction.XXXXXX")
trap 'rm -f "$METADATA"' EXIT

cargo_args=(
  metadata
  --format-version 1
  --all-features
  --manifest-path "$MANIFEST"
)
if [[ $USE_LOCKED == true ]]; then
  cargo_args+=(--locked)
fi

cargo "${cargo_args[@]}" >"$METADATA"
python3 "$VALIDATOR" "$POLICY" "$METADATA"
