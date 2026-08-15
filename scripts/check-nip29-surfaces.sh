#!/usr/bin/env bash
# #1122 (PROTOCOL-NIP29OPERATIONS-012/013): structural surface evidence, on
# every one of the four surfaces an app can call NIP-29 from, that:
#
#   (012) each of the nine NIP-29-owned named operations on `Group` takes
#         only semantic fields plus the retained engine/author capability --
#         never a raw kind number, a tag name, a relay, or a route; and
#   (013) those nine names are the ONLY named operation composers a group
#         offers -- no chat composer, no reaction composer, on any surface.
#         An app that wants either builds the event itself with the ordinary
#         kind-blind `publish` escape hatch, which is DELIBERATELY
#         excluded from this check for that reason (so is
#         `read`/`validateContext`/`on`/`group`, which are not operations at
#         all).
#
# This complements, and deliberately does not re-derive,
# `scripts/check-nip29-ownership.sh` (gate 838/1033's structural door/kind-
# ownership contract for the Rust crates only). That gate proves the Rust
# door shape; this one proves the exhaustive nine-name operation catalogue
# and its exact per-surface parameter shape, across Rust, the FFI, Swift AND
# Kotlin -- the width #1122 asks for.
#
# #1124 (PROTOCOL-WHATTHEAPPNEVERDOES-002/003) widens (012)'s exact-shape
# check to the group's own general escape hatch and read door --
# `publish`/`read` -- so "no write or read operation accepts a per-call
# relay, route, or raw context tag" is proven for EVERY way an app can reach
# a group write or read, not only the nine named operations.
# `on`/`group`/`observeRecords` stay excluded, as before: naming hosts once at
# scope construction is the one legal exception this claim is not about.
#
# #1653 renamed this from check-nip29-operation-catalogue.sh to
# check-nip29-surfaces.sh and absorbed two scripts that were also proving
# claims across these same four surfaces, folding a five-script NIP-29 gate
# cluster (~1,070 lines, three separate workflow files) down to two:
#
#   - check-nip29-read-door.sh's whole read-lifecycle claim (below) --
#     ported onto a GLOB over the facade files, `crates/nmp/src/nip29/*.rs`,
#     rather than the hardcoded array read-door.sh used, which silently
#     omitted `groups.rs` from its own facade scan.
#   - check-nip29-group-list-ownership.sh's non-vacuous requirements (the
#     tolerant-parser and fabricated-input-falsifier presence checks, plus
#     the four typed group-list actions) -- its zero-hit tombstone bans
#     (a retired decoder-door name, a retired protocol-specific
#     lifecycle-noun family, a retired NIP-51 component key) were dropped
#     rather than carried over; per #1639's own audit they matched nothing
#     in the repository at deletion time.
#
# Two fail-open holes this absorption fixes rather than inherits:
#
#   - `owner_block` used to stop at the FIRST brace-balanced match of its
#     start regex, so a SECOND `impl Group { ... }` block (legal, idiomatic
#     Rust) could declare a tenth operation invisible to every check below.
#     It now keeps scanning to the end of the file and concatenates every
#     matching block.
#   - `check_signature_shape` banned `kind`/`relay`/`route`/`tag` parameters
#     case-sensitively but not `host`, so `join_request(host: String, ...)`
#     passed. It now also bans `host`, case-insensitively.
#
# #1074 evidence for PROTOCOL-NIP29OPERATIONS-012/013
# (features/groups/one-typed-group-door.feature).
set -euo pipefail

SCRIPT_PATH=${BASH_SOURCE[0]}
SCRIPT_DIR=${SCRIPT_PATH%/*}
[[ $SCRIPT_DIR != "$SCRIPT_PATH" ]] || SCRIPT_DIR=.
source "$SCRIPT_DIR/lib/require-commands.sh" || exit 2
require_commands awk grep || exit 2

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

fail() { echo "nip29-surfaces: $*" >&2; exit 1; }

RUST_FACADE=crates/nmp/src/nip29/group.rs
RUST_GROUPS=crates/nmp/src/nip29/groups.rs
RUST_FFI=crates/nmp-ffi/src/nip29.rs
SWIFT=Packages/NMP/Sources/NMP/NIP29.swift
KOTLIN=Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/NIP29.kt

for path in "$RUST_FACADE" "$RUST_GROUPS" "$RUST_FFI" "$SWIFT" "$KOTLIN"; do
  [[ -f $path ]] || fail "required surface file is missing: $path"
done

# Extract the exact signature text (the fn/func/fun line through the line
# whose trimmed content opens with the closing paren) for one method name in
# one file, keyword-delimited so a name that is a prefix/suffix of another
# (there are none today, but belt and suspenders) cannot cross-match.
signature_block() {
  local file=$1 keyword=$2 name=$3
  awk -v keyword="$keyword" -v name="$name" '
    BEGIN { capturing = 0 }
    capturing {
      print
      if ($0 ~ /\)[ \t]*(->|:|throws)/) { exit }
      next
    }
    $0 ~ ("(^|[^A-Za-z0-9_])" keyword "[ \t]+" name "\\(") {
      capturing = 1
      print
      if ($0 ~ /\)[ \t]*(->|:|throws)/) { exit }
    }
  ' "$file"
}

# The exact lines of EVERY type/impl block that owns the operations
# (`impl Group`/`impl FfiGroup`/`class NMPGroup`) -- brace-depth bounded, so
# a sibling type in the same file (`FfiRelayScope`, `FfiGroupPredicate`,
# `NmpGroupReceiptStream`, the free `member_list_includes` function, ...)
# never leaks into the operation-name enumeration below.
#
# #1653 (hole 1): this used to `exit` at the first brace-balanced match, so
# a SECOND `impl Group { ... }` block elsewhere in the file -- legal,
# idiomatic Rust; inherent methods on a type may be declared across as many
# `impl` blocks as the author likes -- was invisible to every check below,
# and a tenth named operation with a raw kind/relay parameter could hide
# there undetected. It now resets `started` instead of exiting, so scanning
# continues to the end of the file and concatenates every matching block.
owner_block() {
  local file=$1 start_regex=$2
  awk -v start_regex="$start_regex" '
    BEGIN { depth = 0; started = 0 }
    started {
      print
      depth += gsub(/\{/, "{")
      depth -= gsub(/\}/, "}")
      if (depth == 0) { started = 0 }
      next
    }
    $0 ~ start_regex {
      started = 1
      depth += gsub(/\{/, "{")
      depth -= gsub(/\}/, "}")
      print
      if (depth == 0) { started = 0 }
    }
  ' "$file"
}

# Every method/function NAME the owner block declares with `keyword`, one
# per line -- used both to enumerate the actual named-operation set and to
# hunt for a decoy composer under a name this script was not told to expect.
declared_names() {
  local file=$1 keyword=$2 start_regex=$3
  owner_block "$file" "$start_regex" |
    grep -oE "(^|[^A-Za-z0-9_])${keyword}[ \t]+[A-Za-z_][A-Za-z0-9_]*\\(" |
    sed -E "s/^.*${keyword}[ \t]+//; s/\\($//"
}

check_signature_shape() {
  local file=$1 keyword=$2 name=$3
  local block
  block=$(signature_block "$file" "$keyword" "$name")
  [[ -n $block ]] || fail "$file: named operation \`$name\` has no resolvable signature"
  # #1653 (hole 2): the field-name half of this check was missing `host` and
  # was case-sensitive, so `join_request(host: String, ...)` passed --
  # exactly the parameter class this check exists to forbid. Both are fixed:
  # `host` is banned alongside `kind`/`relay`/`route`/`tag`, and the match is
  # now case-insensitive (`-qiE`), matching `check_no_routing_parameter`'s
  # already-case-insensitive field-name check below.
  if grep -qE '(^|[^A-Za-z0-9_])(Kind|RelayUrl|Tag|IndexedTagName)([^A-Za-z0-9_]|$)' <<<"$block" ||
    grep -qiE '(^|[^A-Za-z0-9_])(kind|relay|route|tag|host)[ \t]*:' <<<"$block"; then
    printf '%s\n' "$block" >&2
    fail "$file: named operation \`$name\` takes a raw kind, tag, relay, route or host parameter"
  fi
}

# Narrower than check_signature_shape, deliberately: `publish` is the
# general kind-blind escape hatch, so a caller-supplied kind and raw
# tags are LEGAL there (that is the whole reason the escape hatch exists --
# "Legal general capabilities that must remain legal", #1124). What must
# still never appear is a per-call relay, route or host -- naming one of
# those again after `RelayScope`/`Group` construction is exactly what
# WHATTHEAPPNEVERDOES-002/003 forbids.
check_no_routing_parameter() {
  local file=$1 keyword=$2 name=$3
  local block
  block=$(signature_block "$file" "$keyword" "$name")
  [[ -n $block ]] || fail "$file: infrastructure operation \`$name\` has no resolvable signature"
  if grep -qE '(^|[^A-Za-z0-9_])RelayUrl([^A-Za-z0-9_]|$)' <<<"$block" ||
    grep -qiE '(^|[^A-Za-z0-9_])(relay|route|host)[A-Za-z0-9_]*[ \t]*:' <<<"$block"; then
    printf '%s\n' "$block" >&2
    fail "$file: infrastructure operation \`$name\` takes a per-call relay, route or host parameter"
  fi
}

# (012): the nine names, on every surface, take semantic fields plus the
# retained engine/author capability alone.
RUST_OPS=(join_request leave_request add_users remove_users edit_metadata delete_event create_group delete_group create_invite)
CAMEL_OPS=(joinRequest leaveRequest addUsers removeUsers editMetadata deleteEvent createGroup deleteGroup createInvite)

for op in "${RUST_OPS[@]}"; do
  check_signature_shape "$RUST_FACADE" "pub fn" "$op"
  check_signature_shape "$RUST_FFI" "pub fn" "$op"
done
for op in "${CAMEL_OPS[@]}"; do
  check_signature_shape "$SWIFT" "public func" "$op"
  check_signature_shape "$KOTLIN" "fun" "$op"
done

# #1124 (PROTOCOL-WHATTHEAPPNEVERDOES-002/003): the group's own general
# escape hatch and its read door -- `publish`/`read`, on
# every surface -- take no per-call relay, route or raw context/tag
# parameter EITHER, the same shape check (012) already proves for the nine
# named operations. `on` is deliberately excluded: it is the one legal place
# an app names hosts, ONCE, at scope construction (#1033); this claim is
# that a group WRITE or READ never takes one again, not that host
# construction is illegal. `group`/`observe` are likewise excluded: a
# group id (a `String`) and a `GroupPredicate` value are not a relay, route,
# or raw context tag.
RUST_INFRA_OPS=(publish read)
CAMEL_INFRA_OPS=(publish read)

for op in "${RUST_INFRA_OPS[@]}"; do
  check_no_routing_parameter "$RUST_FACADE" "pub fn" "$op"
  check_no_routing_parameter "$RUST_FFI" "pub fn" "$op"
done
for op in "${CAMEL_INFRA_OPS[@]}"; do
  check_no_routing_parameter "$SWIFT" "public func" "$op"
  check_no_routing_parameter "$KOTLIN" "fun" "$op"
done

# #1281: `Groups` is the SEVERAL-group write context. It offers no named
# operation at all -- every 9000-9022 action names one group by definition --
# so (013) has nothing to enumerate on it. It offers exactly ONE door, and
# (012)'s routing rule must hold for it: naming hosts once at scope
# construction stays the one legal exception, and `groups(ids)` is a narrowing
# exactly as `group(id)` is, not a per-call route.
RUST_GROUPS_INFRA_OPS=(publish)

for op in "${RUST_GROUPS_INFRA_OPS[@]}"; do
  check_no_routing_parameter "$RUST_GROUPS" "pub fn" "$op"
done

# (013): those nine names are the WHOLE named-operation catalogue -- no
# surface may declare a tenth. The group's live records projection
# (`observe`/`observeRecords`) is excluded for the same reason `read` is: it
# is the group's READ door, kind-blind over the three records NIP-29 defines
# to describe a group, and not one of the nine things NIP-29 lets you DO.
# `read`/`validateContext`(`_context`)/
# `publish`/`on`/`group`/predicate composition
# are infrastructure, not NIP-29-owned operations, and `publish` is
# deliberately kind-BLIND (the escape hatch this scenario says an app
# wanting a chat or reaction composer must use instead) -- excluded here
# for that reason, not by oversight. `mint` is `publish`'s own private
# routing/identity seam and is not a surface at all.
#
# `intent`/`signed_intent`/`publish_signed` were excluded here for the same
# reason until #1292 DELETED them: the group hands back no unpublished
# intent and publishes no caller-signed bytes, so their absence from this
# list is what makes reintroducing one fail as an undeclared tenth
# operation. Do not re-add them.
RUST_EXCLUDED="new|read|read_branches|observe|observe_records|validate_context|publish|mint"
CAMEL_EXCLUDED="read|observeRecords|validateContext|publish"

check_exact_catalogue() {
  local file=$1 keyword=$2 start_regex=$3 excluded=$4
  shift 4
  local expected=("$@")
  local actual
  actual=$(declared_names "$file" "$keyword" "$start_regex" | grep -vE "^(${excluded})\$" | sort -u)
  local want
  want=$(printf '%s\n' "${expected[@]}" | sort -u)
  if [[ $actual != "$want" ]]; then
    fail "$file: named-operation catalogue is not exactly the nine modeled operations. got:
$actual
expected:
$want"
  fi
}

RUST_FACADE_OWNER='^impl Group \{'
RUST_FFI_OWNER='^impl FfiGroup \{'
SWIFT_OWNER='^public final class NMPGroup: @unchecked Sendable \{'
KOTLIN_OWNER='^class NMPGroup internal constructor'

check_exact_catalogue "$RUST_FACADE" "pub fn" "$RUST_FACADE_OWNER" "$RUST_EXCLUDED" "${RUST_OPS[@]}"
check_exact_catalogue "$RUST_FFI" "pub fn" "$RUST_FFI_OWNER" "$RUST_EXCLUDED" "${RUST_OPS[@]}"
check_exact_catalogue "$SWIFT" "public func" "$SWIFT_OWNER" "$CAMEL_EXCLUDED" "${CAMEL_OPS[@]}"
check_exact_catalogue "$KOTLIN" "fun" "$KOTLIN_OWNER" "$CAMEL_EXCLUDED" "${CAMEL_OPS[@]}"

# (013) belt and suspenders: no chat/reaction-shaped composer name anywhere
# in these four files, independent of the exact-catalogue diff above (which
# would already have failed on a tenth METHOD -- this also catches a decoy
# free function, constant, or type name).
decoys=$(grep -RInE 'compose_?[Cc]hat|composeChat|ChatMessage|GroupReply|[Rr]eaction[A-Za-z]*\(|sendReaction|composeReaction' \
  "$RUST_FACADE" "$RUST_FFI" "$SWIFT" "$KOTLIN" || true)
if [[ -n $decoys ]]; then
  printf '%s\n' "$decoys"
  fail "a chat- or reaction-shaped composer name appeared on a NIP-29 group surface"
fi

# ---------------------------------------------------------------------------
# #1123/#1233, absorbed from the deleted check-nip29-read-door.sh (#1653): a
# NIP-29 group/relay-scope value owns no READ LIFECYCLE of its own -- no
# socket, no subscription bookkeeping, no retry, no second cancellation
# semantics -- on every surface an app can hold one from. The one observation
# that DOES exist must be the engine's own: Rust opens
# `Engine::observe_async`, and each native wrapper drains that Rust-owned
# handle rather than minting a lifecycle of its own.
RUST_NIP29=(crates/nmp-nip29/src/context.rs crates/nmp-nip29/src/discovery.rs crates/nmp-nip29/src/operations.rs crates/nmp-nip29/src/records.rs)
RUST_FACADE_RECORDS=crates/nmp/src/nip29/records.rs
RUST_FACADE_FILES=(crates/nmp/src/nip29/*.rs)

for path in "${RUST_NIP29[@]}" "${RUST_FACADE_FILES[@]}"; do
  [[ -f $path ]] || fail "required read-door surface file is missing: $path"
done

# The engine-free crate holds no engine, so it can hold no observation at all.
if grep -nE 'fn observe|fn subscribe|fn stream' "${RUST_NIP29[@]}"; then
  fail "the engine-free NIP-29 crate grew an observation; it mints values only"
fi
# Nowhere on any surface may a group value grow subscribe/stream lifecycle
# vocabulary of its own beside the one observation.
if grep -nE 'fn subscribe|fn stream' "${RUST_FACADE_FILES[@]}"; then
  fail "a group-shaped subscribe/stream lifecycle appeared in the facade Rust crate"
fi
if grep -nE 'pub fn subscribe|pub fn stream' "$RUST_FFI"; then
  fail "a group-shaped subscribe/stream lifecycle appeared in the Rust FFI surface"
fi
if grep -nE 'func subscribe|func stream' "$SWIFT"; then
  fail "a group-shaped subscribe/stream lifecycle appeared in the Swift surface"
fi
if grep -nE 'fun subscribe|fun stream' "$KOTLIN"; then
  fail "a group-shaped subscribe/stream lifecycle appeared in the Kotlin surface"
fi

grep -qF 'engine.observe_async(query, None)' "$RUST_FACADE_RECORDS" ||
  fail "the facade group-records observation no longer opens the engine's own subscription"
# Prose is stripped first: these files EXPLAIN, in words, that they own no
# transport or retry, and that explanation must not trip the check.
lifecycle=$(grep -vhE '^\s*(//|\*)' "${RUST_FACADE_FILES[@]}" | grep -nE 'Transport|RelayPool|reconnect|\bretry\(|thread::spawn' || true)
if [[ -n $lifecycle ]]; then
  printf '%s\n' "$lifecycle"
  fail "a group value grew a read lifecycle of its own; the engine owns that"
fi
grep -qF 'NmpGroupRecordsStream' "$RUST_FFI" ||
  fail "the FFI group-records handle is missing"
grep -qF 'observeRecords' "$SWIFT" ||
  fail "the Swift group-records observation is missing"
grep -qF 'observeRecords' "$KOTLIN" ||
  fail "the Kotlin group-records observation is missing"
for native in "$SWIFT" "$KOTLIN"; do
  native_lifecycle=$(grep -vE '^\s*(//|\*|/\*)' "$native" |
    grep -nE 'URLSession|WebSocket|OkHttp|Timer\.scheduled|reconnect' || true)
  if [[ -n $native_lifecycle ]]; then
    printf '%s\n' "$native_lifecycle"
    fail "a native group surface grew a transport/retry lifecycle of its own: $native"
  fi
done

# ---------------------------------------------------------------------------
# #1551/#863, absorbed from the deleted check-nip29-group-list-ownership.sh
# (#1653): the non-vacuous requirements only -- the ones that assert
# something IS present and would fail if it were deleted. The file's
# zero-hit tombstone bans (a retired decoder-door name, a retired
# protocol-specific lifecycle-noun family, a retired NIP-51 component key)
# matched nothing in the repository at deletion time per #1639's own audit
# and were not carried forward.
GROUP_LIST_SOURCES=(
  crates/nmp-nip29/src/simple_groups.rs
  crates/nmp-ffi/src/nip29_simple_groups.rs
  Packages/NMP/Sources/NMP/NIP29SimpleGroups.swift
  Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/NIP29SimpleGroups.kt
)
ACTION_SOURCES=(
  crates/nmp/src/nip29/group_list_writes.rs
  crates/nmp-ffi/src/nip29_simple_groups.rs
  Packages/NMP/Sources/NMP/NIP29SimpleGroups.swift
  Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/NIP29SimpleGroups.kt
)
for path in "${GROUP_LIST_SOURCES[@]}" "${ACTION_SOURCES[@]}"; do
  [[ -f $path ]] || fail "required group-list surface file is missing: $path"
done

# #1653 addition, beyond the "8 non-vacuous requirements" scope: not a
# zero-hit tombstone by #1639's audit, but a claim three production doc
# comments (Packages/NMP/Sources/NMP/NIP29SimpleGroups.swift,
# Packages/NMPKotlin/.../NIP29SimpleGroups.kt, crates/nmp-ffi/src/
# nip29_simple_groups.rs) assert is "mechanically kept absent" by name. Two
# shapes were shipped and withdrawn here before: an authoritative-sounding
# `decode_*` door (genuinely zero-hit today, not carried forward) and a
# public observation-qualified `ObservedSimpleGroupsList` minted from a frame
# proof. Dropping this ban silently would have made those three doc comments
# false rather than merely stale.
if grep -nE \
  'ObservedSimpleGroups|QualifiedSimpleGroups|SimpleGroupsProjection|CanonicalSimpleGroups|AuthoritativeSimpleGroups|project_observed_simple_groups|projectObservedSimpleGroups|SimpleGroupsWitness|SimpleGroupsProof|FrameProof|ObservationHandle|AuthorityToken|FfiObservation|FfiFrame([^A-Za-z0-9_]|$)' \
  "${GROUP_LIST_SOURCES[@]}"; then
  fail "a derived NIP-29 group-list authority/lifecycle/frame-proof API appeared"
fi

# Tolerance must be explicit in the name at every layer, on all four
# platforms.
for requirement in \
  'crates/nmp-nip29/src/simple_groups.rs:parse_simple_groups_list_tolerant' \
  'crates/nmp-ffi/src/nip29_simple_groups.rs:parse_simple_groups_list_tolerant' \
  'Packages/NMP/Sources/NMP/NIP29SimpleGroups.swift:parseSimpleGroupsListTolerant' \
  'Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/NIP29SimpleGroups.kt:parseSimpleGroupsListTolerant'
do
  file=${requirement%%:*}
  symbol=${requirement#*:}
  grep -qF -- "$symbol" "$file" || fail "$file is missing $symbol"
done

# The tolerant-parser falsifiers must keep proving that fabricated,
# wrong-kind input preserves evidence instead of becoming authority.
grep -qF 'tolerant_parse_of_fabricated_input_yields_plain_evidence_not_authority' \
  crates/nmp-nip29/src/simple_groups.rs ||
  fail "direct-Rust fabricated-input falsifier is missing"
grep -qF 'tolerant_parser_preserves_evidence_even_for_fabricated_wrong_kind_row' \
  crates/nmp-ffi/src/nip29_simple_groups.rs ||
  fail "FFI fabricated-wrong-kind falsifier is missing"
grep -qF 'testTolerantParserPreservesEvidenceForFabricatedWrongKindRow' \
  Packages/NMP/Tests/NMPTests/NIP29SimpleGroupsTests.swift ||
  fail "Swift fabricated-wrong-kind falsifier is missing"
grep -qF 'tolerantParserPreservesEvidenceForFabricatedWrongKindRow' \
  Packages/NMPKotlin/src/test/kotlin/com/nmp/sdk/NIP29SimpleGroupsTest.kt ||
  fail "Kotlin fabricated-wrong-kind falsifier is missing"

# #1653 addition, beyond group-list-ownership.sh's own scope: the four typed
# group-list actions, present by name on every platform. Dropping this loop
# along with the rest of the deleted file would have been a real coverage
# regression -- unlike the tombstone bans above, it is a positive assertion
# that would fail the moment one of these symbols were deleted, and nothing
# else in the surviving two scripts checks it.
for symbol in add_group_to_list remove_group_from_list add_relay_in_use remove_relay_in_use; do
  grep -qF "pub fn $symbol" crates/nmp/src/nip29/group_list_writes.rs ||
    fail "direct Rust group-list action is missing: $symbol"
  grep -qF "pub fn $symbol" crates/nmp-ffi/src/nip29_simple_groups.rs ||
    fail "FFI group-list action is missing: $symbol"
done
for symbol in addGroupToList removeGroupFromList addRelayInUse removeRelayInUse; do
  grep -qF "func $symbol" Packages/NMP/Sources/NMP/NIP29SimpleGroups.swift ||
    fail "Swift group-list action is missing: $symbol"
  grep -qF "fun NMPEngine.$symbol" Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/NIP29SimpleGroups.kt ||
    fail "Kotlin group-list action is missing: $symbol"
done

echo "nip29-surfaces: ok"
