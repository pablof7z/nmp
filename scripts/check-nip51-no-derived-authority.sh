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
# noun appears, or if the explicit relay-scope NIP-29 selection seam
# disappears.
#
# #1033 widened NIP-29 browsing from one pinned host to a caller-supplied
# relay SET (`nip29::on(hosts)` -> `RelayScope`, narrowed with `.group(id)`);
# `group_discovery_demand(host)` is gone, no alias. The invariant this script
# polices is unchanged by that widening: the host(s) an app browses a group
# with are its own explicit typed input, never harvested from NIP-51 parser
# output by the boundary itself.
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
  crates/nmp-nip29/src/discovery.rs
  crates/nmp-ffi/src/nip29.rs
  Packages/NMP/Sources/NMP/NIP29.swift
  Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/NIP29.kt
)
for required in "${NIP51_SOURCES[@]}" "${NIP29_SOURCES[@]}"; do
  [[ -f $required ]] || fail "required path is missing: $required"
done

# 1. Tolerance must be explicit in the name at every layer. The former
#    `decode*` spelling hid that caller-constructible input was accepted.
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
  fail "derived NIP-51 authority/lifecycle API appeared"
fi

# 3. No frame proof or second observation handle may be reachable from the
#    NIP-51 parsing files themselves -- the kind:10009 read is the ordinary
#    `LiveQuery`, and parsing adds no handle beside it.
if grep -nE \
  'FrameProof|ObservationHandle|AuthorityToken|FfiObservation|FfiFrame([^A-Za-z0-9_]|$)' \
  "${NIP51_SOURCES[@]}"; then
  fail "NIP-51 parsing acquired an observation lifecycle or proof surface"
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

# 5. NIP-29 browsing takes an EXPLICIT typed relay set the app selected. If
#    any of these constructors stops taking its hosts as caller-supplied
#    input, host selection has started deriving from somewhere -- and a
#    tolerant parser result is the only nearby candidate. #1033 replaced the
#    single-host `group_discovery_demand(host)` with a caller-supplied SET
#    (`nip29::on(hosts) -> RelayScope`), so the seam is now fallible -- an
#    app-supplied set can be empty -- rather than the old infallible
#    single-host door; that widening is exactly what makes it worth guarding
#    here too.
[[ ! -e crates/nmp-nip29/src/demand.rs ]] ||
  fail "the deleted single-host crates/nmp-nip29/src/demand.rs reappeared"
tombstones=$(census 'group_discovery_demand|groupDiscoveryDemand')
if [[ -n $tombstones ]]; then
  printf '%s\n' "$tombstones"
  fail "the deleted single-host group_discovery_demand seam reappeared"
fi
grep -qF 'pub fn on(hosts: impl IntoIterator<Item = RelayUrl>) -> Result<RelayScope, RelayScopeError>' \
  crates/nmp/src/nip29/mod.rs ||
  fail "direct-Rust NIP-29 explicit relay-scope selection seam is missing"
grep -qF 'pub fn on(hosts: Vec<String>) -> Result<Arc<Self>, FfiError>' crates/nmp-ffi/src/nip29.rs ||
  fail "FFI NIP-29 explicit relay-scope selection seam is missing"
grep -qE 'func on\(' Packages/NMP/Sources/NMP/NIP29.swift ||
  fail "Swift NIP-29 explicit relay-scope selection seam is missing"
grep -qE 'fun on\(' Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/NIP29.kt ||
  fail "Kotlin NIP-29 explicit relay-scope selection seam is missing"
grep -qF 'nip29_browsing_still_demands_an_explicitly_supplied_host' crates/nmp-ffi/src/nip51.rs ||
  fail "the FFI falsifier proving NIP-29 browsing takes an explicit host is missing"

# 6. The workload nouns stay exactly two. A NIP-51 file must not export a
#    third query/intent-shaped noun of its own.
if grep -nE 'pub struct .*Query|pub struct .*Intent|struct .*Observation' "${NIP51_SOURCES[@]}"; then
  fail "NIP-51 parsing introduced a third workload noun"
fi

echo "nip51-no-derived-authority: ok"
