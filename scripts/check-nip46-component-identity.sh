#!/usr/bin/env bash
# #952 artifact falsifier. The ordinary three-library mode proves a core-only
# build differs from a matched package set and that the matched pair agrees.
# Apple PR qualification may use --matched-only because the Ubuntu provider
# gate retains the real package-set mismatch proof.

set -euo pipefail

SCRIPT_PATH=${BASH_SOURCE[0]}
SCRIPT_DIR=${SCRIPT_PATH%/*}
[[ $SCRIPT_DIR != "$SCRIPT_PATH" ]] || SCRIPT_DIR=.
source "$SCRIPT_DIR/lib/require-commands.sh" || exit 2
require_commands cmp comm grep mktemp rm sort strings tr wc || exit 2

MODE=full
if [[ ${1:-} == --matched-only ]]; then
  MODE=matched
  shift
fi
if [[ "$MODE" == matched ]]; then
  if [[ $# -ne 2 ]]; then
    echo "usage: $0 --matched-only MATCHED_CORE_LIBRARY MATCHED_PROVIDER_LIBRARY" >&2
    exit 2
  fi
  MATCHED_CORE_LIBRARY=$1
  MATCHED_PROVIDER_LIBRARY=$2
else
  if [[ $# -ne 3 ]]; then
    echo "usage: $0 CORE_ONLY_LIBRARY MATCHED_CORE_LIBRARY MATCHED_PROVIDER_LIBRARY" >&2
    exit 2
  fi
  CORE_ONLY_LIBRARY=$1
  MATCHED_CORE_LIBRARY=$2
  MATCHED_PROVIDER_LIBRARY=$3
fi

fail() { echo "nip46-component-identity: $*" >&2; exit 1; }

libraries=("$MATCHED_CORE_LIBRARY" "$MATCHED_PROVIDER_LIBRARY")
if [[ "$MODE" == full ]]; then
  libraries=("$CORE_ONLY_LIBRARY" "${libraries[@]}")
fi
for library in "${libraries[@]}"; do
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

identity_set "$MATCHED_CORE_LIBRARY" "$TMP/matched-core"
identity_set "$MATCHED_PROVIDER_LIBRARY" "$TMP/matched-provider"

cmp -s "$TMP/matched-core" "$TMP/matched-provider" ||
  fail "matched core and provider identity sets differ"

if [[ "$MODE" == full ]]; then
  identity_set "$CORE_ONLY_LIBRARY" "$TMP/core-only"
  if [[ -n $(comm -12 "$TMP/core-only" "$TMP/matched-core") ]]; then
    fail "core-only and matched package-set identities overlap"
  fi
  echo "nip46-component-identity: core-only=$(wc -l < "$TMP/core-only" | tr -d ' ') matched=$(wc -l < "$TMP/matched-core" | tr -d ' ') shared; package-set mismatch refused"
else
  echo "nip46-component-identity: matched=$(wc -l < "$TMP/matched-core" | tr -d ' ') shared"
fi
