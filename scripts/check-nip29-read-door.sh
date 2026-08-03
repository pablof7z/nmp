#!/usr/bin/env bash
# #1123 (PROTOCOL-READSTHROUGHTHEONEDOOR-002): structural surface evidence
# that a NIP-29 group/relay-scope value owns no READ LIFECYCLE of its own --
# no socket, no subscription bookkeeping, no retry, no second cancellation
# semantics -- on every surface an app can hold one from.
#
# #1233 narrowed this from its original form. It used to ban the WORD
# observe/subscribe/stream outright on every surface. That banned the
# group-records projection (`RelayScope::observe`, `Group::observe`, and their
# native mirrors) along with the defect, and the defect it is aimed at is
# narrower: a group value that opens something the engine did not, or that
# grows a parallel lifecycle onto the same mechanism -- the read-side twin of
# the `publish_composed` second write lifecycle #838 deleted.
#
# So what is checked now is that every group-shaped observation routes through
# the ONE engine door and hands back the ONE engine-owned handle, and that no
# transport/retry/reconnect vocabulary appears beside it. `nmp_nip02`'s follow
# observation has exactly this relationship to the same door and is the
# precedent, not an exception.
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
# Each of the files below declares ONLY group/relay-scope/predicate/record
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
RUST_FACADE_RECORDS=crates/nmp/src/nip29/records.rs
RUST_FACADE=(crates/nmp/src/nip29/mod.rs crates/nmp/src/nip29/group.rs crates/nmp/src/nip29/predicate.rs crates/nmp/src/nip29/read.rs "$RUST_FACADE_RECORDS")
RUST_FFI=crates/nmp-ffi/src/nip29.rs
SWIFT=Packages/NMP/Sources/NMP/NIP29.swift
KOTLIN=Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/NIP29.kt

RUST_NIP29+=(crates/nmp-nip29/src/records.rs)

for path in "${RUST_NIP29[@]}" "${RUST_FACADE[@]}" "$RUST_FFI" "$SWIFT" "$KOTLIN"; do
  [[ -f $path ]] || fail "required surface file is missing: $path"
done

# The engine-free crate holds no engine, so it can hold no observation at all.
if grep -nE 'fn observe|fn subscribe|fn stream' "${RUST_NIP29[@]}"; then
  fail "the engine-free NIP-29 crate grew an observation; it mints values only"
fi

# Nowhere on any surface may a group value grow subscribe/stream lifecycle
# vocabulary of its own beside the one observation.
if grep -nE 'fn subscribe|fn stream' "${RUST_FACADE[@]}"; then
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

# The one observation that DOES exist must be the engine's own, on every
# surface: Rust opens `Engine::observe_async`, and each native wrapper drains
# the Rust-owned handle rather than minting a lifecycle of its own.
grep -qF 'engine.observe_async(query, None)' "$RUST_FACADE_RECORDS" ||
  fail "the facade group-records observation no longer opens the engine's own subscription"
# Prose is stripped first: these files EXPLAIN, in words, that they own no
# transport or retry, and that explanation must not trip the check.
lifecycle=$(grep -vhE '^\s*(//|\*)' "${RUST_FACADE[@]}" | grep -nE 'Transport|RelayPool|reconnect|\bretry\(|thread::spawn' || true)
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

echo "nip29-read-door: ok"
