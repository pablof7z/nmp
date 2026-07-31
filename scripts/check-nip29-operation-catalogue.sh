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
#         kind-blind `publish`/`publishSigned` escape hatch, which is
#         DELIBERATELY excluded from this check for that reason (so is
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

fail() { echo "nip29-operation-catalogue: $*" >&2; exit 1; }

RUST_FACADE=crates/nmp/src/nip29/group.rs
RUST_FFI=crates/nmp-ffi/src/nip29.rs
SWIFT=Packages/NMP/Sources/NMP/NIP29.swift
KOTLIN=Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/NIP29.kt

for path in "$RUST_FACADE" "$RUST_FFI" "$SWIFT" "$KOTLIN"; do
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

# The exact lines of the one type/impl block that owns the operations
# (`impl Group`/`impl FfiGroup`/`class NMPGroup`) -- brace-depth bounded, so
# a sibling type in the same file (`FfiRelayScope`, `FfiGroupPredicate`,
# `NmpGroupReceiptStream`, the free `member_list_includes` function, ...)
# never leaks into the operation-name enumeration below.
owner_block() {
  local file=$1 start_regex=$2
  awk -v start_regex="$start_regex" '
    BEGIN { depth = 0; started = 0 }
    started {
      print
      depth += gsub(/\{/, "{")
      depth -= gsub(/\}/, "}")
      if (depth == 0) { exit }
      next
    }
    $0 ~ start_regex {
      started = 1
      depth += gsub(/\{/, "{")
      depth -= gsub(/\}/, "}")
      print
      if (depth == 0) { exit }
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
  if grep -qE '(^|[^A-Za-z0-9_])(Kind|RelayUrl|Tag|IndexedTagName)([^A-Za-z0-9_]|$)' <<<"$block" ||
    grep -qE '(^|[^A-Za-z0-9_])(kind|relay|route|tag)[ \t]*:' <<<"$block"; then
    printf '%s\n' "$block" >&2
    fail "$file: named operation \`$name\` takes a raw kind, tag, relay or route parameter"
  fi
}

# (012): the nine names, on every surface, take semantic fields plus the
# retained engine/author capability alone.
RUST_OPS=(join_request leave_request add_user remove_user edit_metadata delete_event create_group delete_group create_invite)
CAMEL_OPS=(joinRequest leaveRequest addUser removeUser editMetadata deleteEvent createGroup deleteGroup createInvite)

for op in "${RUST_OPS[@]}"; do
  check_signature_shape "$RUST_FACADE" "pub fn" "$op"
  check_signature_shape "$RUST_FFI" "pub fn" "$op"
done
for op in "${CAMEL_OPS[@]}"; do
  check_signature_shape "$SWIFT" "public func" "$op"
  check_signature_shape "$KOTLIN" "fun" "$op"
done

# (013): those nine names are the WHOLE named-operation catalogue -- no
# surface may declare a tenth. `read`/`validateContext`(`_context`)/
# `publish`/`publishSigned`/`on`/`group`/`groupsWhere`/predicate composition
# are infrastructure, not NIP-29-owned operations, and `publish`/
# `publishSigned` are deliberately kind-BLIND (the escape hatch this
# scenario says an app wanting a chat or reaction composer must use
# instead) -- excluded here for that reason, not by oversight.
RUST_EXCLUDED="new|read|read_branches|validate_context|publish|publish_signed|intent|through_the_one_door"
CAMEL_EXCLUDED="read|validateContext|publish|publishSigned"

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

echo "nip29-operation-catalogue: ok"
