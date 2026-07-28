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
#   *_v1        storage table and schema identifiers (packed_postings_v1,
#               index_cardinality_meta_v1) -- versioned names, not shims.
#   *_old       English in test names ("closes old before new").
#   compat*     UniFFI ABI metadata between core and provider components
#               (nmp-nip46-ffi) -- build-time type identity, not versioning.
#   legacy*     interop with FOREIGN implementations. `nmp-nip46`'s
#               `legacy_secret` accepts an older NIP-46 request shape so
#               third-party signers can migrate. That is legitimate: those
#               signers are strangers we genuinely cannot update. The rule
#               bans compatibility with OUR OWN retired surface, not interop
#               with other people's protocol implementations.
#
# Distinguishing those cases needs judgement, so review owns them. This gate
# owns the part that does not: a declared deprecation.
set -euo pipefail

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
