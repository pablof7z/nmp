#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
ROOT=${SURFACE_ROOT:-$(git rev-parse --show-toplevel)}
BASE_REF=${SURFACE_BASE_REF:-}
CHECKER=${SURFACE_CHECKER:-$SCRIPT_DIR/check-surface-governance.sh}

[[ -n "$BASE_REF" ]] || {
  echo "surface-regeneration-governance: SURFACE_BASE_REF is required" >&2
  exit 2
}
git -C "$ROOT" cat-file -e "$BASE_REF^{commit}" 2>/dev/null || {
  echo "surface-regeneration-governance: base commit is unavailable: $BASE_REF" >&2
  exit 2
}

# The owner signal exists only while the base is the pre-catalog legacy state.
# The checker still derives and requires the exact two-record transition, so a
# missing catalog alone cannot authorize any other bootstrap shape.
if git -C "$ROOT" cat-file -e \
  "$BASE_REF:tools/surface-component-catalog/Cargo.toml" 2>/dev/null; then
  unset SURFACE_OWNER_BOOTSTRAP
else
  export SURFACE_OWNER_BOOTSTRAP=1
fi

exec "$CHECKER" "$@"
