#!/usr/bin/env bash
# #838: keep NIP-29's context ownership separate from C7 chat schema and
# client mention/notification policy.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

fail() { echo "nip29-ownership: $*" >&2; exit 1; }

required=(
  crates/nmp-nip29/src/publication.rs
  crates/nmp-nip29/src/demand.rs
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

# NIP-29 may preserve a foreign C7 `q` row, but it may not define kind:9,
# chat replies, mention materialization, notification p rows, or a fixed
# content-kind catalog.
for source in crates/nmp-nip29/src/*.rs; do
  found=$(
    awk '/^#\[cfg\(test\)\]/{exit} {print}' "$source" |
      grep -nE 'CHAT_KIND|Kind::from\(9|compose_chat|GroupReply|recipient_pubkeys|group_content_demand|\[9[^0-9]+30315\]' ||
      true
  )
  if [[ -n $found ]]; then
    printf '%s:%s\n' "$source" "$found"
    fail "NIP-29 re-acquired foreign chat/content-schema ownership"
  fi
done

# `previous` may appear only as a reserved authority that contextualization
# rejects. No tuple/window constructor or tag emitter may exist.
if grep -nE 'GroupTimelineEvidence|PREVIOUS_MAX|from_events|Tag::parse\(\["previous"' \
  crates/nmp-nip29/src/*.rs; then
  fail "caller-mintable previous authority reappeared"
fi

grep -qF 'pub fn contextualize_group_event(' crates/nmp-nip29/src/publication.rs ||
  fail "complete-draft NIP-29 contextualization seam is missing"
grep -qF 'foreign_kind_and_schema_survive_except_for_appended_h' \
  crates/nmp-nip29/src/publication.rs ||
  fail "foreign-schema preservation falsifier is missing"
grep -qF 'publication_never_synthesizes_previous' crates/nmp-nip29/src/publication.rs ||
  fail "no-previous falsifier is missing"

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

# With no supported NIP-29 write operation, the universal write plane must
# not retain a speculative single-host route solely for tests.
found=$(grep -RInE 'HostAuthority|PinnedHost|pinned-host-hex' \
  crates/nmp-grammar crates/nmp-engine crates/nmp crates/nmp-ffi || true)
if [[ -n $found ]]; then
  printf '%s\n' "$found"
  fail "dead NIP-29-only write authority remains reachable"
fi

echo "nip29-ownership: ok"
