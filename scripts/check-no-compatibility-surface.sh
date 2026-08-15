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
#   *_vN        not a compatibility surface by itself. Production durable
#               tables are unsuffixed; SCHEMA_VERSION is the one epoch
#               authority. Remaining *_vN spellings are foreign layouts
#               (nostrdb), protocol names, or measurement baselines.
#   *_old       English in local names ("older before newer"), not a surface.
#   legacy*     one-way import of FOREIGN material. The rule bans
#               compatibility with OUR OWN retired surface; it does not ban
#               reading someone else's bytes once, on the way in.
#
# The native SDK trees are not scanned either. A Rust attribute cannot appear
# in them -- `git ls-files -- 'Packages/*.rs'` is empty, and those trees hold
# only Swift and Kotlin, which spell the marker `@available(*, deprecated)`
# and `@Deprecated`. A `--include='*.rs'` grep over `Packages/` could never
# match on any input, so it reported "ok" for a property it had not tested.
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

echo "no-compatibility-surface: ok"
