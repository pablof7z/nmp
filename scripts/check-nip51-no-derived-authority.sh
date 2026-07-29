#!/usr/bin/env bash
# #863: NIP-51 Simple-groups parsing is tolerant and OBSERVATIONAL. A parser
# result is data, never authority.
#
# Two shapes have already been shipped and withdrawn here:
#
#   1. `decode_simple_groups_list(FfiRow)` -- a tolerant tag/content reader
#      whose name sounded authoritative, so a caller-constructed row could be
#      mistaken for canonical NIP-51 state and fed into NIP-29 host selection.
#   2. a public observation-qualified `ObservedSimpleGroupsList` minted from a
#      frame proof -- a speculative protocol-specific lifecycle/capability on
#      top of `LiveQuery`, with no operation that needed it.
#
# Prose cannot keep either out (bug-class-ledger type-over-convention
# doctrine). This script is the mechanism: it fails the build if the
# authoritative-sounding door reopens, if any observation-qualified NIP-51
# noun appears, or if the explicit-host NIP-29 selection seam disappears.
set -euo pipefail

SCRIPT_PATH=${BASH_SOURCE[0]}
SCRIPT_DIR=${SCRIPT_PATH%/*}
[[ $SCRIPT_DIR != "$SCRIPT_PATH" ]] || SCRIPT_DIR=.
source "$SCRIPT_DIR/lib/require-commands.sh" || exit 2
require_commands dirname git grep xargs || exit 2

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

fail() { echo "nip51-no-derived-authority: $*" >&2; exit 1; }

# Portability note: plain POSIX `grep` only. GitHub's ubuntu-latest runner has
# no `ripgrep`, and this gate must run with no toolchain and no setup step.
#
# The searched corpus is `git ls-files` over crates/ and Packages/ -- tracked
# sources only, so build output and uniffi-generated bindings can neither hide
# a violation nor manufacture one.
# `xargs`'s "some invocation exited 1" status differs between GNU and BSD, so
# a match is detected by captured OUTPUT, never by exit status.
census() { git ls-files -- crates Packages | xargs grep -nE "$1" || true; }

# Every layer that projects Simple-groups parsing must actually exist. A
# missing file would otherwise turn each search below into a vacuous pass.
NIP51_SOURCES=(
  crates/nmp-nip51/src/simple_groups.rs
  crates/nmp-ffi/src/nip51.rs
  Packages/NMP/Sources/NMP/NIP51.swift
  Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/NIP51.kt
)
NIP29_SOURCES=(
  crates/nmp-nip29/src/demand.rs
  crates/nmp-ffi/src/nip29.rs
  Packages/NMP/Sources/NMP/NIP29.swift
  Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/NIP29.kt
)
for required in "${NIP51_SOURCES[@]}" "${NIP29_SOURCES[@]}"; do
  [[ -f $required ]] || fail "required path is missing: $required"
done

# 1. Tolerance must be explicit in the name at every layer. The former
#    `decode*` spelling hid that caller-constructible input was accepted.
# (`docs/surface-change-log.md` is append-only history and is deliberately
# not scanned: it records the withdrawn spelling as a fact of the past.)
found=$(census 'decode_simple_groups_list|decodeSimpleGroupsList')
if [[ -n $found ]]; then
  printf '%s\n' "$found"
  fail "obsolete authoritative-sounding decoder door reappeared"
fi

for requirement in \
  'crates/nmp-nip51/src/simple_groups.rs:parse_simple_groups_list_tolerant' \
  'crates/nmp-ffi/src/nip51.rs:parse_simple_groups_list_tolerant' \
  'Packages/NMP/Sources/NMP/NIP51.swift:parseSimpleGroupsListTolerant' \
  'Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/NIP51.kt:parseSimpleGroupsListTolerant'
do
  file=${requirement%%:*}
  symbol=${requirement#*:}
  grep -qF -- "$symbol" "$file" || fail "$file is missing $symbol"
done

# 2. Parsing has no consumer operation that needs authority today, so it may
#    not mint a reusable protocol-specific proof, canonical wrapper,
#    lifecycle, or authority token -- anywhere in the workspace or the SDKs.
found=$(census 'ObservedSimpleGroups|QualifiedSimpleGroups|SimpleGroupsProjection|CanonicalSimpleGroups|AuthoritativeSimpleGroups|project_observed_simple_groups|projectObservedSimpleGroups|SimpleGroupsWitness|SimpleGroupsProof')
if [[ -n $found ]]; then
  printf '%s\n' "$found"
  fail "derived NIP-51 authority/lifecycle surface appeared"
fi

# 3. No frame proof or second observation handle may be reachable from the
#    NIP-51 parsing files themselves -- the kind:10009 read is the ordinary
#    `LiveQuery`, and parsing adds no handle beside it.
if grep -nE \
  'FrameProof|ObservationHandle|AuthorityToken|FfiObservation|FfiFrame([^A-Za-z0-9_]|$)' \
  "${NIP51_SOURCES[@]}"; then
  fail "NIP-51 parsing acquired an observation lifecycle/proof surface"
fi

# 4. The tolerant-parser falsifiers must keep proving that fabricated,
#    wrong-kind input preserves evidence instead of becoming authority.
grep -qF 'tolerant_parse_of_fabricated_input_yields_plain_evidence_not_authority' \
  crates/nmp-nip51/src/simple_groups.rs ||
  fail "direct-Rust fabricated-input falsifier is missing"
grep -qF 'tolerant_parser_preserves_evidence_even_for_fabricated_wrong_kind_row' \
  crates/nmp-ffi/src/nip51.rs ||
  fail "FFI fabricated-wrong-kind falsifier is missing"
grep -qF 'testTolerantParserPreservesEvidenceForFabricatedWrongKindRow' \
  Packages/NMP/Tests/NMPTests/NIP51Tests.swift ||
  fail "Swift fabricated-wrong-kind falsifier is missing"
grep -qF 'tolerantParserPreservesEvidenceForFabricatedWrongKindRow' \
  Packages/NMPKotlin/src/test/kotlin/com/nmp/sdk/NIP51Test.kt ||
  fail "Kotlin fabricated-wrong-kind falsifier is missing"

# 5. NIP-29 browsing takes an EXPLICIT typed host the app selected. If any of
#    these signatures stops taking its host as a parameter, host selection has
#    started deriving from somewhere -- and a tolerant parser result is the
#    only nearby candidate.
grep -qF 'pub fn group_discovery_demand(host: RelayUrl)' crates/nmp-nip29/src/demand.rs ||
  fail "direct-Rust NIP-29 explicit-host selection seam is missing"
grep -qF 'pub fn group_discovery_demand(host: String)' crates/nmp-ffi/src/nip29.rs ||
  fail "FFI NIP-29 explicit-host selection seam is missing"
grep -qF 'func groupDiscoveryDemand(host: String)' Packages/NMP/Sources/NMP/NIP29.swift ||
  fail "Swift NIP-29 explicit-host selection seam is missing"
grep -qF 'fun groupDiscoveryDemand(' Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/NIP29.kt ||
  fail "Kotlin NIP-29 explicit-host selection seam is missing"

# 6. The workload nouns stay exactly two. A NIP-51 file must not export a
#    third query/intent-shaped noun of its own.
if grep -nE 'pub struct .*Query|pub struct .*Intent|struct .*Observation' "${NIP51_SOURCES[@]}"; then
  fail "NIP-51 parsing introduced a third workload noun"
fi

echo "nip51-no-derived-authority: ok"
