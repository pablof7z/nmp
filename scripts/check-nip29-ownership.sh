#!/usr/bin/env bash
# #838: keep NIP-29's context ownership separate from C7 chat schema and
# client mention/notification policy.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

fail() { echo "nip29-ownership: $*" >&2; exit 1; }

required=(
  crates/nmp-nip29/src/group.rs
  crates/nmp-nip29/src/demand.rs
  crates/nmp/src/group.rs
  crates/nmp-nipc7/src/lib.rs
  crates/nmp-ffi/src/nip29.rs
  Packages/NMP/Sources/NMP/NIP29.swift
  Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/NIP29.kt
)
for path in "${required[@]}"; do
  [[ -f $path ]] || fail "required path is missing: $path"
done

[[ ! -e crates/nmp-nip29/src/message.rs ]] ||
  fail "NIP-29 kind:9 message composer reappeared"
[[ ! -e crates/nmp-nip29/src/send.rs ]] ||
  fail "obsolete raw NIP-29 write-intent composer reappeared"

grep -qF '"crates/nmp-nipc7"' Cargo.toml ||
  fail "nmp-nipc7 is not an independently selectable workspace crate"
if grep -nE '(^|[[:space:]])nmp([[:space:]]*=|-engine|-router|-resolver|-store)' \
  crates/nmp-nip29/Cargo.toml crates/nmp-nipc7/Cargo.toml; then
  fail "pure NIP-29/C7 schema crates gained core or mechanism dependencies"
fi

# NIP-29 may preserve a C7 `q` row, but it may not define kind:9,
# chat replies, mention materialization, notification p rows, or a fixed
# content-kind catalog.
#
# NIP-29's OWN kinds (9000-9022 join/leave/moderation) are this crate's, per
# #989, so the kind ban is exact rather than prefix-shaped: `Kind::from(9)`
# is refused while `Kind::from(JOIN_REQUEST)` at 9021 is allowed. Because a
# named constant would otherwise launder kind 9 past an exact match, a
# constant bound to 9 (`= 9;`) is refused too. Prefer adding a kind here only
# when NIP-29 itself defines it.
for source in crates/nmp-nip29/src/*.rs; do
  found=$(
    awk '/^#\[cfg\(test\)\]/{exit} {print}' "$source" |
      grep -nE 'CHAT_KIND|Kind::from\(9\)|=[[:space:]]*9;|compose_chat|GroupReply|recipient_pubkeys|group_content_demand|\[9[^0-9]+30315\]' ||
      true
  )
  if [[ -n $found ]]; then
    printf '%s:%s\n' "$source" "$found"
    fail "NIP-29 re-acquired chat/content-schema ownership it does not have"
  fi
done

# `previous` may appear only as a reserved authority that contextualization
# rejects. No tuple/window constructor or tag emitter may exist.
if grep -nE 'GroupTimelineEvidence|PREVIOUS_MAX|from_events|Tag::parse\(\["previous"' \
  crates/nmp-nip29/src/*.rs; then
  fail "caller-mintable previous authority reappeared"
fi

# #977 revises this block. It used to require the free function
# `contextualize_group_event` and the carrier struct `GroupPublication` in
# `crates/nmp-nip29/src/publication.rs`. Both are DELETED: they were the
# build-but-cannot-deliver half of the old world -- nothing in the workspace
# could route what they returned -- and their duties moved inside `Group`,
# which mints the `h` row AND the `Explicit([host])` route as one closed
# value (`docs/internals/nip29/group-publication.md` §9).
#
# What the gate pins now is the door, not the deleted seam: the `Group` type,
# its two write-intent constructors (unsigned-contextualize and
# presigned-validate), and BOTH PROPERTIES the old falsifiers proved, carried
# over verbatim under their own names.
grep -qF 'pub struct Group {' crates/nmp-nip29/src/group.rs ||
  fail "the NIP-29 Group door is missing"
grep -qF 'pub fn write_intent(' crates/nmp-nip29/src/group.rs ||
  fail "the unsigned group write-intent constructor is missing"
grep -qF 'pub fn signed_write_intent(' crates/nmp-nip29/src/group.rs ||
  fail "the pre-signed group write-intent constructor is missing"
grep -qF 'draft_kind_and_schema_survive_except_for_appended_h' \
  crates/nmp-nip29/src/group.rs ||
  fail "draft schema preservation falsifier is missing"
grep -qF 'publication_never_synthesizes_previous' crates/nmp-nip29/src/group.rs ||
  fail "no-previous falsifier is missing"

# The tombstones themselves. #977 deleted `contextualize_group_event` and
# `GroupPublication` outright -- no alias, no deprecation window
# (`docs/internals/conventions/no-backwards-compatibility.md`) -- so neither
# spelling may return anywhere a caller could reach, including in a test that
# asserts one stays gone.
tombstones=$(grep -RInE 'contextualize_group_event|GroupPublication' \
  crates/ Packages/ skills/ || true)
if [[ -n $tombstones ]]; then
  printf '%s\n' "$tombstones"
  fail "a deleted NIP-29 publication spelling reappeared"
fi

# `Group` mints a read DEMAND and nothing else: the one read door is
# `Engine::observe`, and a group-shaped stream would be the read-side twin of
# the `publish_composed` second write lifecycle #838 deleted.
if grep -nE 'fn observe|fn subscribe|fn stream' \
  crates/nmp-nip29/src/*.rs crates/nmp/src/group.rs; then
  fail "a second read door for groups appeared; LiveQuery/Engine::observe is the one"
fi

# One publish door. The group binding composes an intent and hands it over;
# it never grows a write lifecycle of its own.
grep -qF 'engine.publish(intent)' crates/nmp/src/group.rs ||
  fail "the group binding no longer routes through the one publish door"
if grep -nE 'publish_composed' crates/nmp/src/group.rs; then
  fail "a second write lifecycle for groups appeared"
fi

# C7 itself owns the exact kind and q reply schema, independently of NIP-29.
grep -qF 'pub const CHAT_KIND: u16 = 9;' crates/nmp-nipc7/src/lib.rs ||
  fail "C7 kind:9 ownership is missing"
grep -qF 'Tag::parse(["q"' crates/nmp-nipc7/src/lib.rs ||
  fail "C7 q-reply construction is missing"
grep -qF 'reply_uses_q_and_no_e_p_h_or_previous_rows' crates/nmp-nipc7/src/lib.rs ||
  fail "C7 q-only reply falsifier is missing"

# The superseded monolithic native projection must stay deleted. This source
# corpus deliberately excludes append-only surface history.
native_sources=(
  crates/nmp-ffi/src
  Packages/NMP/Sources/NMP
  Packages/NMP/Tests/NMPTests
  Packages/NMPKotlin/src/main
  Packages/NMPKotlin/src/test
)
found=$(grep -RInE \
  'group_content_demand|groupContentDemand|group_message_intent|groupMessageIntent|FfiGroupReplyParent|GroupReplyParent|GroupSendIntent|NoActiveAccount|noActiveAccount' \
  "${native_sources[@]}" || true)
if [[ -n $found ]]; then
  printf '%s\n' "$found"
  fail "superseded NIP-29 native surface reappeared"
fi

# A removed wire/journal spelling must not survive ANYWHERE, including in a
# test that asserts it stays unreadable. Asserting a dead approach is still
# encoding awareness of it; positive and negative awareness are both awareness.
# The invariant those tests proved -- an uninterpretable persisted routing
# fails closed without dropping the obligation -- is proved instead by
# `malformed_persisted_routing_fails_closed_without_dropping_the_obligation`,
# which uses a generic undecodable string and names no dead vocabulary.
#
# #972 retires three more journal spellings into this same clause:
# `author-outbox`, `private-narrow-hex:`, and `nip65-bootstrap-hex:`. The
# durable vocabulary is `auto` and `explicit-hex:`, and a row spelled any
# other way is unreadable by the generic rule, not by a per-spelling one.
dead_spellings=$(grep -RInE 'pinned-host-hex|to-inboxes:|author-outbox|private-narrow-hex|nip65-bootstrap-hex' \
  crates/nmp-grammar crates/nmp crates/nmp-ffi || true)
if [[ -n $dead_spellings ]]; then
  printf '%s\n' "$dead_spellings"
  fail "a removed routing spelling reappeared -- delete it, do not assert it"
fi

# #972 revises the clause that used to sit here.
#
# It banned `HostAuthority`/`PinnedHost` on #838's premise that "no supported
# general-purpose or NIP-29 operation can currently route an arbitrary write
# to one selected relay". That premise is dead: publishing to chosen relays is
# now a first-class general capability (`WriteRouting::Explicit`),
# app-constructible on every platform, and NIP-29 is one consumer of it rather
# than its justification. A grep guarding a capability that should exist is
# not a tripwire, it is sediment
# (`docs/internals/routing/removed-routes.md` §3.3).
#
# What replaces it is a POSITIVE pin on what the reversal must not have
# loosened: the routing vocabulary the design deleted must never come back, in
# any spelling, anywhere an app or SDK can reach. The two dead never-built
# names ride along, for the same reason as above -- they must simply never
# return.
# (`AuthorOutbox` excludes the unrelated read-side `SourceAuthority::
# AuthorOutboxes`, which this design does not touch.)
removed_routing_names='AuthorOutbox([^e]|$)|PrivateNarrow|NarrowOnly|PrivateRoute|RelayListBootstrap|HostAuthority|PinnedHost'
found=$(grep -RInE "$removed_routing_names" \
  crates/nmp-grammar/src crates/nmp/src crates/nmp-ffi/src \
  Packages/NMP/Sources Packages/NMPKotlin/src/main || true)
if [[ -n $found ]]; then
  printf '%s\n' "$found"
  fail "a deleted routing spelling came back; the vocabulary is Auto and Explicit"
fi

# The ownership half is untouched by the reversal and still holds: routing
# policy for a group belongs to nmp-nip29, never to the engine crates.
echo "nip29-ownership: ok"
