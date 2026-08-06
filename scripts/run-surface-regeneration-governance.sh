#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
ROOT=${SURFACE_ROOT:-$(git rev-parse --show-toplevel)}
BASE_REF=${SURFACE_BASE_REF:-}
# The checker this wrapper runs is its own sibling, never a path a caller
# supplies: the two travel together out of the base commit (#1186).
CHECKER="$SCRIPT_DIR/check-surface-governance.sh"

# Wiring this wrapper wrong stops the gate before it judges anything, so it
# exits with the checker's malfunction code rather than a code a caller could
# mistake for a verdict about the head (#1264).
MALFUNCTION_EXIT=70

[[ -n "$BASE_REF" ]] || {
  echo "surface-regeneration-governance-malfunction: SURFACE_BASE_REF is required" >&2
  exit "$MALFUNCTION_EXIT"
}
git -C "$ROOT" cat-file -e "$BASE_REF^{commit}" 2>/dev/null || {
  echo "surface-regeneration-governance-malfunction: base commit is unavailable: $BASE_REF" >&2
  exit "$MALFUNCTION_EXIT"
}

exec "$CHECKER" "$@"
