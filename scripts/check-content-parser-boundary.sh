#!/usr/bin/env bash
set -euo pipefail

SCRIPT_PATH=${BASH_SOURCE[0]}
SCRIPT_DIR=${SCRIPT_PATH%/*}
[[ $SCRIPT_DIR != "$SCRIPT_PATH" ]] || SCRIPT_DIR=.
source "$SCRIPT_DIR/lib/require-commands.sh" || exit 2
require_commands dirname rg || exit 2

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$REPO"

# The dependency half of this boundary is NOT checked here. `nmp-content` is
# classified `pure-extension` in `scripts/dependency-direction-policy.json`,
# a role whose `may_reach` is `generic-value` and `pure-extension` only --
# so `nmp` (facade) and `nmp-store`/`nmp-router`/`nmp-resolver`/
# `nmp-transport` (generic-mechanism) are already unreachable, proved by
# `scripts/check-dependency-direction.py` against the resolved cargo graph
# and fail-closed on any unclassified package. A second `cargo tree` walk
# over five hardcoded names tested a strict subset of that, more slowly, and
# would not have noticed a sixth mechanism crate.
#
# What is left below is what the policy cannot express: which TYPES and
# FUNCTIONS live on either side of the boundary.

if rg -n \
  'HydrationPolicy|ClaimDecision|ResolutionDecision|ReferenceDemandPlan|decode_profile|ProfileMetadata|decode_article|struct Article' \
  crates/nmp-content/src; then
  echo "error: nmp-content regained acquisition policy or protocol-codec ownership" >&2
  exit 1
fi

if rg -n \
  'ReferenceDemandPlan|reference_demand_plan|FfiReferenceDemandPlan|NostrReferenceDemandPlan' \
  crates Packages apps; then
  echo "error: compatibility locator planner vocabulary still exists in active code" >&2
  exit 1
fi

locator_paths=(
  crates/nmp-grammar/src/nip19.rs
  crates/nmp-ffi/src/entity.rs
  crates/nmp-ffi/src/content.rs
)

if rg -n \
  'ReferenceDemandPlan|reference_demand_plan|SourceAuthority|classify_relay_host|AuthorOutboxes|PinnedRelays|canonical.*helper|helper.*canonical' \
  "${locator_paths[@]}"; then
  echo "error: pure locator decoding regained acquisition or relay-routing policy" >&2
  exit 1
fi

for removed_policy_path in \
  crates/nmp-grammar/src/reference.rs \
  crates/nmp-ffi/src/reference.rs \
  Packages/NMP/Sources/NMPUI/ReferenceDemand.swift \
  Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/Reference.kt; do
  if [[ -e "$removed_policy_path" ]]; then
    echo "error: compatibility locator planner still exists: $removed_policy_path" >&2
    exit 1
  fi
done

for locator_path in "${locator_paths[@]}"; do
  if rg -n \
    'nmp_(engine|router|resolver|transport)|crate::reference|nmp::(Demand|LiveQuery|SourceAuthority|Freshness)' \
    "$locator_path"; then
    echo "error: pure locator path depends on acquisition machinery: $locator_path" >&2
    exit 1
  fi
done

echo "content parser boundary: ok"
