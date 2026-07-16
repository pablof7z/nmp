#!/usr/bin/env bash
set -euo pipefail

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$REPO"

tree=$(cargo tree -p nmp-content -e normal --prefix none)
for forbidden in nmp nmp-engine nmp-store nmp-router nmp-resolver nmp-transport; do
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

echo "content parser boundary: ok"
