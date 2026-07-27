#!/usr/bin/env bash
# #952 artifact falsifier. A core-only build and a matched core/provider build
# must use different identities, while both artifacts from the matched Cargo
# package set must embed exactly the same per-target identity set.

set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: $0 CORE_ONLY_LIBRARY MATCHED_CORE_LIBRARY MATCHED_PROVIDER_LIBRARY" >&2
  exit 2
fi

CORE_ONLY_LIBRARY=$1
MATCHED_CORE_LIBRARY=$2
MATCHED_PROVIDER_LIBRARY=$3

fail() { echo "nip46-component-identity: $*" >&2; exit 1; }

for library in "$CORE_ONLY_LIBRARY" "$MATCHED_CORE_LIBRARY" "$MATCHED_PROVIDER_LIBRARY"; do
  [[ -f "$library" ]] || fail "library is missing: $library"
done

TMP=$(mktemp -d "${TMPDIR:-/tmp}/nmp-nip46-component-identity.XXXXXX")
trap 'rm -rf "$TMP"' EXIT

identity_set() {
  local library=$1 output=$2
  strings -a "$library" |
    grep -Eo 'nmp-core-component-v1-[0-9a-f]{64}' |
    sort -u > "$output"
  [[ -s "$output" ]] || fail "no component identity embedded in $library"
}

identity_set "$CORE_ONLY_LIBRARY" "$TMP/core-only"
identity_set "$MATCHED_CORE_LIBRARY" "$TMP/matched-core"
identity_set "$MATCHED_PROVIDER_LIBRARY" "$TMP/matched-provider"

if [[ -n $(comm -12 "$TMP/core-only" "$TMP/matched-core") ]]; then
  fail "core-only and matched package-set identities overlap"
fi
cmp -s "$TMP/matched-core" "$TMP/matched-provider" ||
  fail "matched core and provider identity sets differ"

echo "nip46-component-identity: core-only=$(wc -l < "$TMP/core-only" | tr -d ' ') matched=$(wc -l < "$TMP/matched-core" | tr -d ' ') shared; package-set mismatch refused"
