#!/usr/bin/env bash
# #1105: the app-facing routing vocabulary is EXACTLY two words, on every
# surface, and the spellings the design deleted never come back.
#
# `docs/internals/routing/auto-and-explicit.md` settles the vocabulary at
# `Auto` ("figure out how to route whatever I'm publishing") and `Explicit`
# ("use these exact relays and that is that"). Nothing else is expressible:
# a third word would name a NIP or a strategy, and which strategy claims a
# kind is NMP's own business, decided at send time.
#
# Until this gate existed the claim was carried by prose plus one runtime
# parity test, which proves ONE Explicit path rather than the cardinality of
# the vocabulary. A third variant could therefore appear on one SDK -- or a
# retired spelling could return there -- with every existing check still
# green. What this gate owns:
#
#   1. CARDINALITY, per surface, by enumeration rather than by grep: the
#      Rust grammar enum, the FFI mirror, both public FFI conversion paths,
#      the Swift enum and the Kotlin sealed class each declare exactly the
#      two words and nothing else. Because the sets are exact, "it names no
#      NIP and no strategy" needs no separate rule -- there is no third name
#      to be a NIP or a strategy.
#   2. TOMBSTONES: every retired or never-built routing spelling from #972
#      (`docs/internals/routing/removed-routes.md`) is absent from every
#      source tree an app or SDK can reach -- including from a test that
#      asserts one stays gone, since positive and negative awareness are both
#      awareness. Each one's replacement is one of the two words, and the
#      failure message says which.
#   3. THE GROUP DOOR: no group write operation takes a relay or a routing
#      value. The app hands the group an event; the group mints the `h` row
#      and the `Explicit([host])` route. Stated as a signature rather than as
#      a convention (`docs/internals/nip29/group-publication.md` §8).
#
# The runtime half of #1105 -- the app supplies content only, the host alone
# receives, the author outbox is never contacted -- is
# `crates/nmp/tests/group_publication_door.rs`, because a static check cannot
# observe a delivery. `scripts/test-check-routing-vocabulary.sh` is this
# gate's own falsifier: it mutates a fixture tree (a third SDK variant, a
# returned retired spelling, a relay-taking group verb) and requires each
# mutation to go red.
#
# An optional ROOT argument makes that self-test possible: the checker runs
# against any tree, not only its own checkout.
set -euo pipefail

SCRIPT_PATH=${BASH_SOURCE[0]}
SCRIPT_DIR=${SCRIPT_PATH%/*}
[[ $SCRIPT_DIR != "$SCRIPT_PATH" ]] || SCRIPT_DIR=.
source "$SCRIPT_DIR/lib/require-commands.sh" || exit 2
require_commands awk dirname git grep sort tr || exit 2
source "$SCRIPT_DIR/lib/tracked-corpus.sh" || exit 2

ROOT=${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}
cd "$ROOT"

fail() { echo "routing-vocabulary: $*" >&2; exit 1; }

# ---- surface files -------------------------------------------------------

GRAMMAR=crates/nmp-grammar/src/write.rs
FFI_TYPES=crates/nmp-ffi/src/types.rs
FFI_CONVERT=crates/nmp-ffi/src/convert.rs
SWIFT=Packages/NMP/Sources/NMP/WriteIntent.swift
KOTLIN=Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/WriteIntent.kt
GROUP_DOOR=crates/nmp/src/nip29/group.rs

for path in "$GRAMMAR" "$FFI_TYPES" "$FFI_CONVERT" "$SWIFT" "$KOTLIN" "$GROUP_DOOR"; do
  [[ -f $path ]] || fail "required routing surface is missing: $path"
done

# Everything between a declaration header and the next column-0 `}`.
block() {
  local file=$1 header=$2
  awk -v header="$header" '
    !inside && $0 ~ header { inside = 1; next }
    inside && /^\}/ { exit }
    inside { print }
  ' "$file"
}

# Comment-free view of a block: a doc comment may legitimately discuss a
# relay or a route, and only the code is the surface.
without_comments() {
  grep -vE '^[[:space:]]*(//|\*|/\*)' || true
}

normalize() { sort -u | tr '\n' ' ' | sed 's/ $//'; }

expect_words() {
  local label=$1 expected=$2 actual=$3
  [[ $actual == "$expected" ]] ||
    fail "$label declares [$actual]; the routing vocabulary is exactly [$expected].
       A third word names a NIP or a strategy, and which strategy claims a
       kind is decided at send time, never spelled by the app
       (docs/internals/routing/auto-and-explicit.md)."
}

# ---- 1. cardinality, per surface ----------------------------------------

rust_variants=$(
  block "$GRAMMAR" '^pub enum WriteRouting \{' |
    grep -oE '^    [A-Z][A-Za-z0-9]*' | awk '{print $1}' | normalize
)
expect_words "$GRAMMAR::WriteRouting" "Auto Explicit" "$rust_variants"

ffi_variants=$(
  block "$FFI_TYPES" '^pub enum FfiWriteRouting \{' |
    grep -oE '^    [A-Z][A-Za-z0-9]*' | awk '{print $1}' | normalize
)
expect_words "$FFI_TYPES::FfiWriteRouting" "Auto Explicit" "$ffi_variants"

swift_block=$(block "$SWIFT" '^public enum WriteRouting[:[:space:]]')
swift_cases=$(
  printf '%s\n' "$swift_block" |
    grep -oE '^    case [a-z][A-Za-z0-9]*' | awk '{print $2}' | normalize
)
expect_words "$SWIFT::WriteRouting" "auto explicit" "$swift_cases"

kotlin_block=$(block "$KOTLIN" '^sealed class WriteRouting \{')
kotlin_variants=$(
  printf '%s\n' "$kotlin_block" |
    grep -oE '^    (object|data class) [A-Z][A-Za-z0-9]*' | awk '{print $NF}' | normalize
)
expect_words "$KOTLIN::WriteRouting" "Auto Explicit" "$kotlin_variants"

# ---- 2. the public FFI conversion paths ---------------------------------
#
# Both directions are enumerated, not merely present: a surface can hold two
# variants and still lose one on the way across.

to_ffi=$(
  awk '
    /^pub\(crate\) fn write_routing_to_ffi/ { inside = 1 }
    inside { print }
    inside && /^\}/ { exit }
  ' "$FFI_CONVERT"
)
[[ -n $to_ffi ]] || fail "$FFI_CONVERT no longer projects a routing out to the FFI boundary"
expect_words "write_routing_to_ffi (grammar side)" "Auto Explicit" \
  "$(printf '%s\n' "$to_ffi" | grep -oE 'nmp::WriteRouting::[A-Za-z0-9]+' |
    sed 's/.*:://' | normalize)"
expect_words "write_routing_to_ffi (FFI side)" "Auto Explicit" \
  "$(printf '%s\n' "$to_ffi" | grep -oE 'FfiWriteRouting::[A-Za-z0-9]+' |
    sed 's/.*:://' | normalize)"

from_ffi=$(
  awk '
    /let routing = match intent\.routing \{/ { inside = 1 }
    inside { print; if ($0 ~ /^    \};/) exit }
  ' "$FFI_CONVERT"
)
[[ -n $from_ffi ]] || fail "$FFI_CONVERT no longer accepts a routing from the FFI boundary"
expect_words "FFI intent conversion (FFI side)" "Auto Explicit" \
  "$(printf '%s\n' "$from_ffi" | grep -oE 'FfiWriteRouting::[A-Za-z0-9]+' |
    sed 's/.*:://' | normalize)"
expect_words "FFI intent conversion (grammar side)" "Auto Explicit" \
  "$(printf '%s\n' "$from_ffi" | grep -oE 'GWriteRouting::[A-Za-z0-9]+' |
    sed 's/.*:://' | normalize)"

# The SDKs convert in both directions too, and a Swift/Kotlin exhaustive
# switch over its OWN enum cannot be trusted to be exhaustive over the FFI
# one, so the words each SDK maps are enumerated as well.
expect_words "$SWIFT routing conversion" "auto explicit" \
  "$(printf '%s\n' "$swift_block" | grep -oE 'case (let )?\.[a-z][A-Za-z0-9]*' |
    sed 's/.*\.//' | normalize)"
expect_words "$KOTLIN routing conversion" "Auto Explicit" \
  "$(printf '%s\n' "$kotlin_block" | grep -oE 'FfiWriteRouting\.[A-Za-z0-9]+' |
    sed 's/.*\.//' | normalize)"

# Nothing anywhere in the workspace may NAME a third variant either -- a
# `WriteRouting::Whatever` that no enum declares is a compile error today,
# but a match arm added in the same change as the variant would not be.
if [[ -d crates ]]; then
  used=$(
    grep -RIhoE '(Ffi|G)?WriteRouting::[A-Za-z0-9]+' crates 2>/dev/null |
      sed 's/.*:://' | normalize
  )
  [[ -z $used ]] || expect_words "routing variants used across crates/" "Auto Explicit" "$used"
fi

# ---- 3. tombstones ------------------------------------------------------
#
# retired spelling | what the caller says instead
# `AuthorOutbox`     `Auto` -- the built-in behaviour, with the p-tag fan-out
#                    and app relays the variant never had.
# `PrivateNarrow`    `Explicit` -- the invariants survive, the privacy
# `NarrowOnly`       framing does not: fail-closed is a routing property, and
# `PrivateRoute`     a group host is a public target.
# `RelayListBootstrap` `Explicit` minted by `nmp-nip65`.
# `HostAuthority`    `Explicit([host])` minted by `nmp-nip29`; the authority
# `PinnedHost`       newtype was rejected outright.
# `GroupHost`        `Explicit([host])` minted by `nmp-nip29`.
# `AuthorRelayList`  `Auto` -- it was a partial spelling of it with the kind
#                    hoisted into the enum.
#
# #1334: the corpus is what GIT tracks, never what the working tree happens
# to hold. `grep -RInE` over `Packages/` read
# `Packages/NMP/Sources/NMPFFI/nmp_ffi.swift` -- a gitignored uniffi dump
# (`.gitignore:36`) -- and reported a stale generated binding as a
# resurrected spelling, with the offending text in no tracked file and no
# commit. CI checks out clean, so that file is absent there and the same
# walk was quietly fail-open: false positive locally, no coverage where it
# counted. `scripts/lib/tracked-corpus.sh` (#1178) is the corpus a clean
# checkout would have; it must be called at top level, since a failed
# enumeration inside `$(...)` would exit the subshell and read as "no
# violations found".
scan_pathspecs=()
for candidate in crates Packages skills; do
  [[ -d $candidate ]] && scan_pathspecs+=("$candidate")
done
TRACKED_PATHS=()
if ((${#scan_pathspecs[@]})); then
  tracked_paths "$ROOT" "${scan_pathspecs[@]}" ||
    fail "the tracked corpus could not be read, so the tombstone scan would
       be vacuous -- a gate that scans air is worse than no gate."
fi

tombstone() {
  local pattern=$1 retired=$2 replacement=$3 found
  ((${#TRACKED_PATHS[@]})) || return 0
  found=$(census "$ROOT" "$pattern" "${TRACKED_PATHS[@]}")
  if [[ -n $found ]]; then
    printf '%s\n' "$found"
    fail "the retired routing spelling \`$retired\` came back. What a caller
       says instead is $replacement -- delete the name, do not assert it
       (docs/internals/routing/removed-routes.md;
       docs/internals/conventions/no-backwards-compatibility.md)."
  fi
}

# `AuthorOutbox` excludes the unrelated READ-side `SourceAuthority::
# AuthorOutboxes`, which this design does not touch -- hence the trailing
# "not an `e`" guard, in both the Rust and the SDK casing.
tombstone '[Aa]uthor[Oo]utbox([^e]|$)' 'AuthorOutbox' '`Auto`'
tombstone '[Pp]rivate[Nn]arrow' 'PrivateNarrow' '`Explicit`'
tombstone '[Nn]arrow[Oo]nly' 'NarrowOnly' '`Explicit`'
tombstone '[Pp]rivate[Rr]oute' 'PrivateRoute' '`Explicit`'
tombstone '[Rr]elay[Ll]ist[Bb]ootstrap' 'RelayListBootstrap' '`Explicit`, minted by `nmp-nip65`'
tombstone '[Hh]ost[Aa]uthority' 'HostAuthority' '`Explicit([host])`, minted by `nmp-nip29`'
tombstone '[Pp]inned[Hh]ost' 'PinnedHost' '`Explicit([host])`, minted by `nmp-nip29`'
tombstone '[Gg]roup[Hh]ost' 'GroupHost' '`Explicit([host])`, minted by `nmp-nip29`'
tombstone '[Aa]uthor[Rr]elay[Ll]ist' 'AuthorRelayList' '`Auto`'

# ---- 4. the group door --------------------------------------------------
#
# The app hands the group an event and names nothing else. If a group verb
# ever took a relay or a routing value, `Explicit` would stop being minted
# by the group and start being spelled by the app -- which is the boundary
# #977 built the door for and #1033 preserved when it widened one host to a
# scope.
#
# #1033 moved the door: the `GroupOperations` extension trait over a lower
# `nmp-nip29` type is deleted, and the verbs are INHERENT methods on
# `nmp::nip29::Group`, which retains its scope's hosts privately. The
# invariant is unchanged and is checked the same way, with one necessary
# difference: a trait declaration is signatures only, while an inherent impl
# carries BODIES -- and a body is exactly where `WriteRouting::Explicit` is
# legitimately minted from the retained hosts. Scanning bodies would forbid
# the very mechanism this gate exists to protect, so only the public
# signatures are checked.
group_impl=$(block "$GROUP_DOOR" '^impl Group \{' | without_comments)
[[ -n $group_impl ]] || fail "$GROUP_DOOR no longer declares the inherent Group door"

# Each public signature: from its `pub fn` line up to the line that opens the
# body. `pub(crate)` helpers are not app surface and are deliberately skipped.
group_signatures=$(
  printf '%s\n' "$group_impl" |
    awk '
      /^    pub fn / { insig = 1 }
      insig { print }
      insig && /\{[[:space:]]*$/ { insig = 0 }
    '
)
[[ -n $group_signatures ]] || fail "$GROUP_DOOR declares no public group verbs"

offending=$(printf '%s\n' "$group_signatures" | grep -nE 'RelayUrl|WriteRouting|routing|relay' || true)
if [[ -n $offending ]]; then
  printf '%s\n' "$offending"
  fail "a group write operation takes a relay or a routing value. The group
       carries its hosts from the scope it was narrowed from and mints both
       the route and the \`h\` row itself
       (docs/internals/nip29/group-publication.md §8)."
fi

echo "routing-vocabulary: ok"
