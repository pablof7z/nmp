#!/usr/bin/env bash
# Mutation falsifiers for the routing-vocabulary gate (#1105).
#
# A gate nobody has seen go red is a gate nobody knows works. Each mutation
# here is one of the failures the gate exists to catch -- a third routing word
# on a surface, a retired spelling coming back, a group verb that takes a
# relay -- applied to a fixture copy of the real tree and required to fail
# with the message that names the fault.
set -euo pipefail

ROOT=$(git rev-parse --show-toplevel)
CHECKER="$ROOT/scripts/check-routing-vocabulary.sh"
TEMP_ROOT=$(mktemp -d)
FIXTURE=$TEMP_ROOT/repo
trap 'rm -rf "$TEMP_ROOT"' EXIT

GRAMMAR=crates/nmp-grammar/src/write.rs
FFI_TYPES=crates/nmp-ffi/src/types.rs
FFI_CONVERT=crates/nmp-ffi/src/convert.rs
SWIFT=Packages/NMP/Sources/NMP/WriteIntent.swift
KOTLIN=Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/WriteIntent.kt
GROUP_DOOR=crates/nmp/src/group.rs

fail() {
  echo "routing-vocabulary test: $*" >&2
  exit 1
}

reset_fixture() {
  rm -rf "$FIXTURE"
  local path
  for path in "$GRAMMAR" "$FFI_TYPES" "$FFI_CONVERT" "$SWIFT" "$KOTLIN" "$GROUP_DOOR"; do
    mkdir -p "$FIXTURE/${path%/*}"
    cp "$ROOT/$path" "$FIXTURE/$path"
  done
}

# In-place edit that works the same on macOS and GNU sed.
edit() {
  local expression=$1 file=$2
  sed -i.bak "$expression" "$FIXTURE/$file"
  rm "$FIXTURE/$file.bak"
}

expect_failure() {
  local label=$1 expected=$2 output
  if output=$(bash "$CHECKER" "$FIXTURE" 2>&1); then
    fail "$label mutation unexpectedly passed"
  fi
  grep -Fq -- "$expected" <<<"$output" ||
    fail "$label mutation failed for the wrong reason: $output"
}

# The unmutated fixture must pass, or every red below proves nothing.
reset_fixture
bash "$CHECKER" "$FIXTURE" >/dev/null ||
  fail "the unmutated fixture must pass"

# ---- a third routing word, one surface at a time -------------------------

reset_fixture
edit 's/^    Auto,$/    Auto,\n    Nip29Host(Vec<RelayUrl>),/' "$GRAMMAR"
expect_failure "third Rust grammar variant" "the routing vocabulary is exactly [Auto Explicit]"

reset_fixture
edit 's/^    Auto,$/    Auto,\n    Nip29Host { relays: Vec<String> },/' "$FFI_TYPES"
expect_failure "third FFI variant" "the routing vocabulary is exactly [Auto Explicit]"

reset_fixture
edit 's/^    case auto$/    case auto\n    case nip29Host(relays: [String])/' "$SWIFT"
expect_failure "third Swift case" "the routing vocabulary is exactly [auto explicit]"

reset_fixture
edit 's/^    object Auto : WriteRouting()$/    object Auto : WriteRouting()\n    data class Nip29Host(val relays: List<String>) : WriteRouting()/' \
  "$KOTLIN"
expect_failure "third Kotlin variant" "the routing vocabulary is exactly [Auto Explicit]"

# ---- a word lost on the way across the FFI boundary ----------------------

reset_fixture
edit 's/        nmp::WriteRouting::Auto => FfiWriteRouting::Auto,//' "$FFI_CONVERT"
expect_failure "dropped outbound conversion arm" "write_routing_to_ffi"

reset_fixture
edit 's/        FfiWriteRouting::Auto => GWriteRouting::Auto,//' "$FFI_CONVERT"
expect_failure "dropped inbound conversion arm" "FFI intent conversion"

# ---- a retired spelling coming back --------------------------------------

reset_fixture
edit 's/^    Auto,$/    \/\/\/ The old GroupHost variant, restored.\n    Auto,/' "$GRAMMAR"
expect_failure "restored GroupHost" 'the retired routing spelling `GroupHost` came back'

reset_fixture
edit 's/^    Auto,$/    \/\/\/ Formerly AuthorRelayList(Kind).\n    Auto,/' "$GRAMMAR"
expect_failure "restored AuthorRelayList" 'the retired routing spelling `AuthorRelayList` came back'

reset_fixture
edit 's/^    case auto$/    \/\/\/ Replaces authorOutbox.\n    case auto/' "$SWIFT"
expect_failure "restored AuthorOutbox on Swift" 'the retired routing spelling `AuthorOutbox` came back'

reset_fixture
edit 's/^    object Auto : WriteRouting()$/    \/** was PrivateNarrow *\/\n    object Auto : WriteRouting()/' \
  "$KOTLIN"
expect_failure "restored PrivateNarrow on Kotlin" 'the retired routing spelling `PrivateNarrow` came back'

# The read-side `SourceAuthority::AuthorOutboxes` is a different concept and
# must stay legal -- a tombstone that also bans it would be a false positive.
reset_fixture
edit 's/^    Auto,$/    \/\/\/ Unrelated to SourceAuthority::AuthorOutboxes.\n    Auto,/' "$GRAMMAR"
bash "$CHECKER" "$FIXTURE" >/dev/null ||
  fail "the read-side AuthorOutboxes spelling must not trip the write-routing tombstone"

# ---- the group door taking a relay ---------------------------------------

reset_fixture
edit 's/^    fn leave_request(&self, engine: &Engine) -> Result<GroupReceipts, GroupPublishError>;$/    fn leave_request(\&self, engine: \&Engine, relay: RelayUrl) -> Result<GroupReceipts, GroupPublishError>;/' \
  "$GROUP_DOOR"
expect_failure "group verb taking a relay" "takes a relay or a routing value"

echo "routing-vocabulary test: baseline, eleven mutations, and one negative control passed"
