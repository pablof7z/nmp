#!/usr/bin/env bash
# #952 artifact falsifier. Core identity remains selection-neutral; the
# independently built provider embeds that exact requirement and the same
# crossing-interface identity while retaining its own distinct identity.

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
  local library=$1 pattern=$2 output=$3
  strings -a "$library" |
    grep -Eo "$pattern" |
    sort -u > "$output"
  [[ -s "$output" ]] || fail "no component identity embedded in $library"
}

identity_set "$MATCHED_CORE_LIBRARY" \
  'nmp-core-component-v2-[0-9a-f]{64}' "$TMP/matched-core"
identity_set "$MATCHED_PROVIDER_LIBRARY" \
  'nmp-core-component-v2-[0-9a-f]{64}' "$TMP/provider-requirement"
identity_set "$MATCHED_CORE_LIBRARY" \
  'nmp-component-interface-v2-[0-9a-f]{64}' "$TMP/core-interface"
identity_set "$MATCHED_PROVIDER_LIBRARY" \
  'nmp-component-interface-v2-[0-9a-f]{64}' "$TMP/provider-interface"
identity_set "$MATCHED_PROVIDER_LIBRARY" \
  'nmp-nip46-component-v2-[0-9a-f]{64}' "$TMP/provider-identity"

cmp -s "$TMP/matched-core" "$TMP/provider-requirement" ||
  fail "provider required-core identity differs from standalone core"
cmp -s "$TMP/core-interface" "$TMP/provider-interface" ||
  fail "core and provider crossing-interface identities differ"

if [[ "$MODE" == full ]]; then
  identity_set "$CORE_ONLY_LIBRARY" \
    'nmp-core-component-v2-[0-9a-f]{64}' "$TMP/core-only"
  cmp -s "$TMP/core-only" "$TMP/matched-core" ||
    fail "core identity changed when an optional component was selected"
  echo "nip46-component-identity: selection-neutral core, exact provider requirement, shared interface, distinct provider identity"
else
  echo "nip46-component-identity: exact provider requirement and shared interface"
fi
