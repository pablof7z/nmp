#!/usr/bin/env bash
# #1123 (PROTOCOL-READSTHROUGHTHEONEDOOR-002): structural surface evidence
# that a NIP-29 group/relay-scope value exposes no observe/subscribe/stream
# operation of its own, on every surface an app can hold one from.
#
# `scripts/check-nip29-ownership.sh` already pins this for the two Rust
# crates (`nmp-nip29`, `nmp`'s `nip29` module). This gate widens the SAME
# property to the Rust FFI boundary and both hand-written native wrappers --
# the surfaces #1122's `check-nip29-operation-catalogue.sh` already treats as
# the full width an app can call NIP-29 from. It is a separate script (and a
# separate top-level workflow, per AGENTS.md) rather than an edit to
# `scripts/check-nip29-ownership.sh`'s own CI wiring, because
# `.github/workflows/architecture-gates.yml` is a protected path
# (`docs/internals/conventions/protected-path-signoff.md`).
#
# Each of the four files below declares ONLY group/relay-scope/predicate
# types -- never `Engine`/`NMPEngine`/`FfiEngine` -- so an unqualified
# keyword grep cannot cross-match the engine's own, legitimate `observe`.
set -euo pipefail

SCRIPT_PATH=${BASH_SOURCE[0]}
SCRIPT_DIR=${SCRIPT_PATH%/*}
[[ $SCRIPT_DIR != "$SCRIPT_PATH" ]] || SCRIPT_DIR=.
source "$SCRIPT_DIR/lib/require-commands.sh" || exit 2
require_commands grep || exit 2

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

fail() { echo "nip29-read-door: $*" >&2; exit 1; }

RUST_NIP29=(crates/nmp-nip29/src/context.rs crates/nmp-nip29/src/discovery.rs crates/nmp-nip29/src/operations.rs)
RUST_FACADE=(crates/nmp/src/nip29/mod.rs crates/nmp/src/nip29/group.rs crates/nmp/src/nip29/predicate.rs crates/nmp/src/nip29/read.rs)
RUST_FFI=crates/nmp-ffi/src/nip29.rs
SWIFT=Packages/NMP/Sources/NMP/NIP29.swift
KOTLIN=Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/NIP29.kt

for path in "${RUST_NIP29[@]}" "${RUST_FACADE[@]}" "$RUST_FFI" "$SWIFT" "$KOTLIN"; do
  [[ -f $path ]] || fail "required surface file is missing: $path"
done

if grep -nE 'fn observe|fn subscribe|fn stream' "${RUST_NIP29[@]}" "${RUST_FACADE[@]}"; then
  fail "a second read door for groups appeared in the engine-free or facade Rust crate"
fi
if grep -nE 'pub fn observe|pub fn subscribe|pub fn stream' "$RUST_FFI"; then
  fail "a second read door for groups appeared in the Rust FFI surface"
fi
if grep -nE 'func observe|func subscribe|func stream' "$SWIFT"; then
  fail "a second read door for groups appeared in the Swift surface"
fi
if grep -nE 'fun observe|fun subscribe|fun stream' "$KOTLIN"; then
  fail "a second read door for groups appeared in the Kotlin surface"
fi

echo "nip29-read-door: ok"
