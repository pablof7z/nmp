#!/usr/bin/env bash
# NMP keeps no backwards compatibility. A replaced spelling is deleted in the
# same change -- no alias, no deprecation, no forwarding wrapper.
#
# NMP has no external consumers: every caller of every surface is in this
# workspace or a sibling that moves with it. Compatibility is a tax paid to
# strangers, and there are none.
#
# This gate checks the ONE form of the rule that is mechanically exact.
# Deliberately NOT checked, because the check would be mostly false positives
# and an allowlist is the same maintenance trap as a shim:
#
#   *_vN        storage table identifiers (events_v6, postings_meta_v8).
#               These name the ONE current epoch's tables; they are not a set
#               of readable epochs. SCHEMA_VERSION refuses any store that is
#               not exactly current, and no pre-current decoder exists.
#   *_old       English in local names ("older before newer"), not a surface.
#   legacy*     one-way import of FOREIGN material. The rule bans
#               compatibility with OUR OWN retired surface; it does not ban
#               reading someone else's bytes once, on the way in.
#
# Distinguishing those cases needs judgement, so review owns them. This gate
# owns the part that does not: a declared deprecation.
set -euo pipefail

SCRIPT_PATH=${BASH_SOURCE[0]}
SCRIPT_DIR=${SCRIPT_PATH%/*}
[[ $SCRIPT_DIR != "$SCRIPT_PATH" ]] || SCRIPT_DIR=.
source "$SCRIPT_DIR/lib/require-commands.sh" || exit 2
require_commands dirname grep || exit 2

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

fail() { echo "no-compatibility-surface: $*" >&2; exit 1; }

found=$(grep -RIn --include='*.rs' -E '#\[[[:space:]]*deprecated' crates/ || true)
if [[ -n $found ]]; then
  printf '%s\n' "$found"
  fail "a deprecation marker appeared -- delete the old spelling instead"
fi

# A deprecation cannot hide behind a re-export rename either.
found=$(grep -RIn --include='*.rs' -E '#\[[[:space:]]*deprecated' \
  Packages/NMP/Sources Packages/NMPKotlin/src 2>/dev/null || true)
if [[ -n $found ]]; then
  printf '%s\n' "$found"
  fail "a deprecation marker appeared in a native SDK surface"
fi

echo "no-compatibility-surface: ok"
