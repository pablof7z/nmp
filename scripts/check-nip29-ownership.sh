#!/usr/bin/env bash
# #838: keep NIP-29's context ownership separate from C7 chat schema and
# client mention/notification policy.
#
# #1033 retargeted this gate for the RelayScope/Group facade shape: the
# engine-free crate (`crates/nmp-nip29`) owns per-host vocabulary only --
# kinds, `groups_where_at`/`member_list_includes_at`/`admin_list_includes_at`,
# `group_demand_at`, `contextualize`/`validate_context` -- while the app-facing
# door, the retained relay scope, and the one opaque `WriteIntent` all live in
# the `nmp` facade (`crates/nmp/src/nip29/{mod,group,predicate,read}.rs`).
# `Group::new`/`Group::demand`/`Group::write_intent`, the `GroupOperations`
# extension trait, and `group_discovery_demand` are gone -- no alias, no
# forwarding wrapper. Do not re-add checks that require any of them back.
set -euo pipefail

SCRIPT_PATH=${BASH_SOURCE[0]}
SCRIPT_DIR=${SCRIPT_PATH%/*}
[[ $SCRIPT_DIR != "$SCRIPT_PATH" ]] || SCRIPT_DIR=.
source "$SCRIPT_DIR/lib/require-commands.sh" || exit 2
require_commands awk dirname grep || exit 2

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

fail() { echo "nip29-ownership: $*" >&2; exit 1; }

required=(
  crates/nmp-nip29/src/context.rs
  crates/nmp-nip29/src/discovery.rs
  crates/nmp-nip29/src/operations.rs
  crates/nmp/src/nip29/mod.rs
  crates/nmp/src/nip29/group.rs
  crates/nmp/src/nip29/predicate.rs
  crates/nmp/src/nip29/read.rs
  crates/nmp-nipc7/src/lib.rs
  crates/nmp-ffi/src/nip29.rs
  Packages/NMP/Sources/NMP/NIP29.swift
  Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/NIP29.kt
)
for path in "${required[@]}"; do
  [[ -f $path ]] || fail "required path is missing: $path"
done

# The pre-#1033 single-host files are DELETED, not renamed in place -- a
# lower `Group`/demand door reappearing here would mean the facade seam
# regressed back to a per-crate double door.
for gone in crates/nmp-nip29/src/group.rs crates/nmp-nip29/src/demand.rs \
  crates/nmp/src/group.rs; do
  [[ ! -e $gone ]] || fail "a pre-#1033 single-host path reappeared: $gone"
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

# The engine-free crate mints demand/binding VALUES only. `WriteIntent`
# belongs exclusively to the `nmp` facade door (2026-07-31 authority
# correction): a lower import or return of it would let a caller build an
# event under one scope and route/publish it as though it came from another,
# which is exactly what moving the door up was meant to make unspellable.
# Whole-line comments are stripped first so the crate's own doc prose (which
# explains, in words, that it does NOT own `WriteIntent`) does not trip this.
for source in crates/nmp-nip29/src/*.rs; do
  found=$(grep -vE '^\s*//' "$source" | grep -n 'WriteIntent' || true)
  if [[ -n $found ]]; then
    printf '%s:%s\n' "$source" "$found"
    fail "the engine-free NIP-29 crate referenced WriteIntent; that belongs to the nmp facade only"
  fi
done

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

# `nmp::nip29` must be a real module that owns the door and the intent
# factories, never a bare re-export of the engine-free crate (#1033's
# 2026-07-31 authority correction: the final door moved physically into the
# facade because it needs both the retained scope AND the opaque intent).
grep -qF 'pub mod nip29;' crates/nmp/src/lib.rs ||
  fail "nmp::nip29 must be declared as a real module"
if grep -nF 'pub use nmp_nip29 as nip29' crates/nmp/src/lib.rs; then
  fail "nmp::nip29 regressed to a bare re-export of nmp-nip29"
fi

# No trait indirection between an app and Group's own methods -- #1033
# deleted the GroupOperations extension trait; every read/write verb is
# inherent on the facade Group.
if grep -nE '^\s*(pub\s+)?trait GroupOperations' crates/nmp/src/nip29/*.rs; then
  fail "the deleted GroupOperations extension trait reappeared"
fi

# The RelayScope door: named once, fallible (an app-supplied relay set can be
# empty), and the sole place a discovery predicate is closed against a host.
grep -qF 'pub struct RelayScope {' crates/nmp/src/nip29/mod.rs ||
  fail "the NIP-29 RelayScope door is missing"
grep -qF 'pub fn on(hosts: impl IntoIterator<Item = RelayUrl>) -> Result<RelayScope, RelayScopeError>' \
  crates/nmp/src/nip29/mod.rs ||
  fail "the fallible nip29::on(...) constructor is missing"
grep -qF 'pub fn groups_where(' crates/nmp/src/nip29/mod.rs ||
  fail "RelayScope::groups_where is missing"

# The evidence-scoped discovery predicates -- `member_is`/`admin_is` were
# retired by the issue's own authoritative correction because 39001/39002 are
# optional, possibly-partial lists: inclusion is evidence, absence is not
# evidence of the opposite. Only the evidence-scoped spellings may exist.
grep -qF 'pub fn member_list_includes(' crates/nmp/src/nip29/predicate.rs ||
  fail "member_list_includes is missing"
grep -qF 'pub fn admin_list_includes(' crates/nmp/src/nip29/predicate.rs ||
  fail "admin_list_includes is missing"
grep -qF 'pub enum GroupPredicate' crates/nmp/src/nip29/predicate.rs ||
  fail "GroupPredicate is missing"
for combinator in union intersect minus; do
  grep -qF "pub fn $combinator(" crates/nmp/src/nip29/predicate.rs ||
    fail "GroupPredicate::$combinator is missing"
done

# What the gate pins now is the door, not the deleted seam: the facade
# `Group` type, its two write-intent-minting publish methods (unsigned and
# pre-signed), and BOTH PROPERTIES the old falsifiers proved, carried over
# verbatim under their own names but relocated to where the code now lives.
grep -qF 'pub struct Group {' crates/nmp/src/nip29/group.rs ||
  fail "the NIP-29 Group door is missing"
grep -qF 'pub fn publish(' crates/nmp/src/nip29/group.rs ||
  fail "the unsigned group publish door is missing"
grep -qF 'pub fn publish_signed(' crates/nmp/src/nip29/group.rs ||
  fail "the pre-signed group publish door is missing"
grep -qF 'draft_kind_and_schema_survive_except_for_appended_h' \
  crates/nmp-nip29/src/context.rs ||
  fail "draft schema preservation falsifier is missing"
grep -qF 'the_unsigned_door_never_invents_a_previous_tag' crates/nmp-nip29/src/context.rs ||
  fail "no-previous falsifier is missing"

# The two falsifiers #1033 exists to prove, required by name so a rename
# cannot quietly drop the property it once proved: the whole-set Explicit
# route (never one host, never a fallback, never Auto), and the recursive
# per-host source-stamping through every nested NIP-29-owned demand.
grep -qF 'a_group_write_routes_explicitly_to_every_host_in_the_scope' \
  crates/nmp/src/nip29/group.rs ||
  fail "the whole-set Explicit-routing falsifier is missing"
grep -qF 'scope_stamps_exact_hosts_on_every_nested_nip29_demand' \
  crates/nmp/src/nip29/mod.rs ||
  fail "the recursive per-host source-stamping falsifier is missing"

# The tombstones themselves. #977 deleted `contextualize_group_event` and
# `GroupPublication` outright, and #1033 deletes `group_discovery_demand` and
# its `pinned_demand` helper the same way -- no alias, no deprecation window
# (`docs/internals/conventions/no-backwards-compatibility.md`) -- so none of
# these spellings may return anywhere a caller could reach, including in a
# test that asserts one stays gone. (`docs/surface-change-log.md` and
# `docs/surface/*` are append-only surface history and are deliberately not
# scanned: they record withdrawn spellings as facts of the past.)
tombstones=$(grep -RInE \
  'contextualize_group_event|GroupPublication|group_discovery_demand|groupDiscoveryDemand|pinned_demand' \
  crates/ Packages/ skills/ || true)
if [[ -n $tombstones ]]; then
  printf '%s\n' "$tombstones"
  fail "a deleted NIP-29 publication/discovery spelling reappeared"
fi

# The overclaiming spellings `member_is`/`admin_is` must never exist: they
# claim exact current membership/admin state, which 39001/39002 (optional,
# possibly partial) cannot establish. Bounded so it does not also flag
# `member_list_includes`/`admin_list_includes`.
overclaiming=$(grep -RInE '(^|[^A-Za-z0-9_])(member_is|admin_is|memberIs|adminIs)\(' \
  crates/ Packages/ skills/ || true)
if [[ -n $overclaiming ]]; then
  printf '%s\n' "$overclaiming"
  fail "an overclaiming exact-membership/admin spelling reappeared; use the evidence-scoped name"
fi

# `Group` mints a read declaration and nothing else: the one read door is
# `Engine::observe`, and a group-shaped stream would be the read-side twin of
# the `publish_composed` second write lifecycle #838 deleted.
#
# PROTOCOL-READSTHROUGHTHEONEDOOR-002 widens this from the two Rust-crate
# files to every surface an app can hold a group/relay-scope value from: the
# Rust FFI boundary and both hand-written native wrappers. Each of these
# three files declares ONLY group/relay-scope/predicate types -- never
# `Engine`/`NMPEngine`/`FfiEngine` -- so an unqualified keyword grep cannot
# cross-match the engine's own, legitimate `observe`.
if grep -nE 'fn observe|fn subscribe|fn stream' \
  crates/nmp-nip29/src/*.rs crates/nmp/src/nip29/*.rs; then
  fail "a second read door for groups appeared; LiveQuery/Engine::observe is the one"
fi
if grep -nE 'pub fn observe|pub fn subscribe|pub fn stream' crates/nmp-ffi/src/nip29.rs; then
  fail "a second read door for groups appeared in the Rust FFI surface"
fi
if grep -nE 'func observe|func subscribe|func stream' Packages/NMP/Sources/NMP/NIP29.swift; then
  fail "a second read door for groups appeared in the Swift surface"
fi
if grep -nE 'fun observe|fun subscribe|fun stream' \
  Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/NIP29.kt; then
  fail "a second read door for groups appeared in the Kotlin surface"
fi

# One publish door. The group binding composes an intent and hands it over;
# it never grows a write lifecycle of its own.
grep -qF 'engine.publish(intent)' crates/nmp/src/nip29/group.rs ||
  fail "the group binding no longer routes through the one publish door"
if grep -nE 'publish_composed' crates/nmp/src/nip29/group.rs; then
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

# The retired ROUTING VOCABULARY (`AuthorOutbox`, `PrivateNarrow`,
# `RelayListBootstrap`, `GroupHost`, `AuthorRelayList`, ...) is no longer this
# gate's business. #972 left a name-only grep here; #1105 replaced it with
# `scripts/check-routing-vocabulary.sh`, which owns the whole contract for the
# whole domain: the surviving vocabulary ENUMERATED per surface (Rust, FFI in
# both conversion directions, Swift, Kotlin) as exactly two words, every
# retired spelling tombstoned with the replacement it maps to, and the group
# door proved to take no relay and no routing value. One owner, not two
# half-owners.

# The ownership half is untouched by the reversal and still holds: routing
# policy for a group belongs to nmp-nip29/nmp::nip29, never to the engine
# crates.
echo "nip29-ownership: ok"
