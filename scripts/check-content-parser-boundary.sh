#!/usr/bin/env bash
set -euo pipefail

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$REPO"

tree=$(cargo tree -p nmp-content -e normal --prefix none)
for forbidden in nmp nmp-store nmp-router nmp-resolver nmp-transport; do
  if rg -q "^${forbidden} v" <<<"$tree"; then
    echo "error: nmp-content normal dependency tree contains forbidden engine/mechanism crate: $forbidden" >&2
    exit 1
  fi
done

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
