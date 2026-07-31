#!/usr/bin/env bash
# #877/#952: NIP-46 is a selectable signer provider, never part of the
# universal signer/core/native vocabulary. A provider must prove exact native
# compatibility before it can return a take-once adapter for core installation.

set -euo pipefail

SCRIPT_PATH=${BASH_SOURCE[0]}
SCRIPT_DIR=${SCRIPT_PATH%/*}
[[ $SCRIPT_DIR != "$SCRIPT_PATH" ]] || SCRIPT_DIR=.
source "$SCRIPT_DIR/lib/require-commands.sh" || exit 2

MODE=all
if [[ ${1:-} == "--workflows-only" ]]; then
  MODE=workflows
  shift
fi

if [[ -n ${1:-} ]]; then
  ROOT=$1
else
  require_commands dirname || exit 2
  ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
fi
if [[ "$MODE" == workflows ]]; then
  require_commands grep || exit 2
else
  require_commands cargo find grep tr wc xargs || exit 2
fi
cd "$ROOT"

fail() { echo "nip46-provider-boundary: $*" >&2; exit 1; }

swift_provider_workflow=.github/workflows/macos-qualification.yml
kotlin_provider_workflow=.github/workflows/nip46-provider.yml

check_provider_workflows() {
  [[ -f "$swift_provider_workflow" ]] ||
    fail "Swift provider workflow is missing: $swift_provider_workflow"
  [[ -f "$kotlin_provider_workflow" ]] ||
    fail "Kotlin provider workflow is missing: $kotlin_provider_workflow"

  if grep -qE 'target/nmp-component-build/.*/release/libnmp' \
    "$swift_provider_workflow" "$kotlin_provider_workflow"
  then
    fail "provider workflow still audits mutable Cargo-cache libraries instead of packaged outputs"
  fi

  grep -qF 'find Packages/NMP/NMP.xcframework' "$swift_provider_workflow" ||
    fail "Swift provider workflow does not audit the packaged core XCFramework"
  grep -qF \
    'matched_provider=Packages/NMPNip46/NMPNip46.xcframework/$slice_directory/libnmp_nip46_ffi.a' \
    "$swift_provider_workflow" ||
    fail "Swift provider workflow does not audit the packaged provider XCFramework"
  grep -qF 'scripts/check-nip46-component-identity.sh' "$swift_provider_workflow" ||
    fail "Swift provider workflow does not prove matched component identity"
  grep -qF 'scripts/check-nip46-artifact-inventory.sh' "$swift_provider_workflow" ||
    fail "Swift provider workflow does not audit packaged component inventory"

  grep -qF 'scripts/test-component-identity-build.sh' "$kotlin_provider_workflow" ||
    fail "Kotlin provider workflow does not prove unmanaged release identity is refused"
  grep -qF 'Packages/NMPKotlin/src/main/resources/linux-x86-64/libnmp_ffi.so' \
    "$kotlin_provider_workflow" ||
    fail "Kotlin provider workflow does not audit the packaged core resource"
  grep -qF 'Packages/NMPKotlin/nip46/src/main/resources/linux-x86-64/libnmp_nip46_ffi.so' \
    "$kotlin_provider_workflow" ||
    fail "Kotlin provider workflow does not audit the packaged provider resource"
  grep -qF 'scripts/check-nip46-component-identity.sh' "$kotlin_provider_workflow" ||
    fail "Kotlin provider workflow does not prove matched component identity"
  grep -qF 'scripts/check-nip46-artifact-inventory.sh' "$kotlin_provider_workflow" ||
    fail "Kotlin provider workflow does not audit packaged component inventory"

  grep -qF '  change-routing:' "$kotlin_provider_workflow" ||
    fail "Kotlin provider workflow has no cheap change-routing job"
  grep -qF 'scripts/check-nip46-provider-changes.sh "$BASE_SHA" "$HEAD_SHA"' \
    "$kotlin_provider_workflow" ||
    fail "Kotlin provider workflow does not classify pull-request changes"
  grep -qF '          required=true' "$kotlin_provider_workflow" ||
    fail "Kotlin provider workflow does not default classification to running proofs"
  grep -qF '          fetch-depth: 0' "$kotlin_provider_workflow" ||
    fail "Kotlin provider workflow cannot compare the complete pull-request range"
  if [[ $(grep -cF '    needs: change-routing' "$kotlin_provider_workflow") -ne 2 ]]; then
    fail "both expensive NIP-46 jobs must depend on change routing"
  fi
  if [[ $(grep -cF "    if: needs.change-routing.outputs.required == 'true'" \
    "$kotlin_provider_workflow") -ne 2 ]]; then
    fail "both expensive NIP-46 jobs must skip when change routing proves them unaffected"
  fi
}

check_provider_workflows
if [[ "$MODE" == workflows ]]; then
  echo "nip46-provider-boundary: provider workflow ownership ok"
  exit 0
fi

required_paths=(
  crates/nmp-signer/src/capability.rs
  crates/nmp-local-signer/src/local.rs
  crates/nmp-component-interface/src/signer.rs
  crates/nmp-component-interface/component_identity.rs
  crates/nmp-ffi/build.rs
  crates/nmp-ffi/src/signer.rs
  crates/nmp-nip46/src/nip46.rs
  crates/nmp-nip46-ffi/metadata-audit.rs
  crates/nmp-nip46-ffi/src/signer.rs
  Packages/NMP/Sources/NMP/ProviderComponent.swift
  Packages/NMPNip46/Package.swift
  Packages/NMPNip46/Sources/NMPNip46/RemoteSigner.swift
  Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/ProviderComponent.kt
  Packages/NMPKotlin/nip46/build.gradle.kts
  Packages/NMPKotlin/nip46/src/main/kotlin/com/nmp/sdk/RemoteSigner.kt
  scripts/build-component-release.sh
  scripts/check-nip46-component-identity.sh
)
for path in "${required_paths[@]}"; do
  [[ -f "$path" ]] || fail "required ownership path is missing: $path"
done

# The dependency-light signer contract must not regain provider transport,
# protocol parsing, runtime, or concrete-provider dependencies.
if grep -nEi 'nmp-(nip46|transport|network-policy)|tokio|serde(_json)?|(^|[^a-z])url([^a-z]|$)|tungstenite|features[[:space:]]*=.*nip46' \
  crates/nmp-signer/Cargo.toml; then
  fail "nmp-signer regained a provider/runtime/protocol dependency"
fi
if [[ $(cargo tree -p nmp-signer --edges normal | wc -l | tr -d ' ') != 1 ]]; then
  cargo tree -p nmp-signer --edges normal
  fail "the protocol-neutral signer interface is no longer dependency-free"
fi
if grep -nE 'nmp-nip46|features[[:space:]]*=.*nip46' \
  crates/nmp/Cargo.toml crates/nmp-ffi/Cargo.toml; then
  fail "the canonical Rust or core FFI graph depends on NIP-46"
fi
if cargo tree -p nmp --edges normal | grep -qE 'nmp-nip46($|[[:space:]])'; then
  fail "the canonical Rust facade dependency graph contains NIP-46"
fi
if cargo tree -p nmp-nip46 --edges features |
  grep -q 'nostr feature "nip46"'; then
  fail "the provider enabled rust-nostr's NIP-46 umbrella instead of its exact dependencies"
fi

# Core source and hand-written native SDKs cannot name the provider. Generated
# files and append-only history are outside this corpus by construction.
core_roots=(
  crates/nmp-signer/src
  crates/nmp/src
  crates/nmp-ffi/src
  Packages/NMP/Sources/NMP
  Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk
)
found=$(
  find "${core_roots[@]}" -type f \
    \( -name '*.rs' -o -name '*.swift' -o -name '*.kt' \) -print0 |
    xargs -0 grep -niE 'nip[-_ ]?46|bunker:|bunker_|nostrconnect' || true
)
if [[ -n "$found" ]]; then
  printf '%s\n' "$found"
  fail "core source still names the concrete NIP-46 provider"
fi

# The provider contributes one opaque, take-once adapter. Only the core can
# install it or mint the contextual runtime capability used by provider tasks.
interface_source=crates/nmp-component-interface/src/signer.rs
core_signer_source=crates/nmp-ffi/src/signer.rs
provider_signer_source=crates/nmp-nip46-ffi/src/signer.rs
grep -qF 'pub struct FfiSignerAdapter' "$interface_source" ||
  fail "shared opaque signer adapter is missing"
grep -qF 'pub fn new_signer_adapter(' "$interface_source" ||
  fail "provider adapter preparation door is missing"
grep -qF 'pub struct SignerAdapterRuntime' "$interface_source" ||
  fail "core-minted contextual runtime capability is missing"
grep -qF 'let _entered = handle.enter();' "$interface_source" ||
  fail "provider futures are not entered in their linked Tokio context on every poll"
if rg -q 'core-owner|pub unsafe|[Mm]ailbox|CoreSigner(Port|Lease)|pub fn from_core' \
  "$interface_source" "$core_signer_source" "$provider_signer_source"; then
  fail "deleted mailbox/unsafe authority or public contextual-runtime minting survives"
fi
[[ $(rg -o '\.take_for_install\(\)' "$core_signer_source" | wc -l | tr -d ' ') == 1 ]] ||
  fail "core signer adapter must have exactly one take-for-install site"
grep -qF 'pub(crate) fn install_signer_adapter(' "$core_signer_source" ||
  fail "adapter installation is not sealed inside core"
grep -qF 'pub fn install_signer_adapter(' crates/nmp-ffi/src/facade.rs ||
  fail "NmpEngine does not own the adapter installation door"
if grep -qE 'Handle::current[[:space:]]*\(|tokio::spawn[[:space:]]*\(|runtime::Builder::new' \
  "$provider_signer_source"; then
  fail "provider production code regained ambient or provider-owned runtime authority"
fi
grep -qF 'Arc<dyn nmp_signer::Nip46TaskRuntime>' "$provider_signer_source" ||
  fail "provider NIP-46 child tasks bypass the contextual core scheduler"
if cargo tree -p nmp-nip46-ffi --edges normal |
  grep -qE '(^|[[:space:]])nmp(-ffi)? v'; then
  fail "NIP-46 provider normal graph contains nmp or nmp-ffi"
fi
if cargo tree -p nmp-component-interface --edges normal |
  grep -qE 'nmp-(ffi|nip46)($|[[:space:]])|(^|[[:space:]])nmp v'; then
  fail "selection-neutral component interface contains a core/provider root"
fi
grep -qF 'pub fn nmp_core_component_identity() -> String' crates/nmp-ffi/src/signer.rs ||
  fail "core does not export its plain native component identity"
grep -qF -- '"--unit-graph"' crates/nmp-component-interface/component_identity.rs ||
  fail "component identity does not derive Cargo's transitive unit graph"
grep -qF 'NMP_COMPONENT_BUILD_AUTH' crates/nmp-ffi/build.rs ||
  fail "isolated component target lacks per-build builder authorization"
if grep -qF 'nip46-provider-component' crates/nmp-ffi/Cargo.toml; then
  fail "obsolete provider-selection feature survives in core"
fi
grep -qF 'pub fn verify_nip46_component(' crates/nmp-nip46-ffi/src/signer.rs ||
  fail "NIP-46 provider does not verify package interface and core identity"
grep -qF 'compatibility: Arc<FfiNip46Compatibility>' crates/nmp-nip46-ffi/src/signer.rs ||
  fail "NIP-46 provider construction does not require a compatibility proof"
grep -qF 'macro_metadata::extract_from_library' crates/nmp-nip46-ffi/metadata-audit.rs ||
  fail "adapter-entry falsifier does not inspect UniFFI's compiled export authority"
grep -qF 'proof at input zero' crates/nmp-nip46-ffi/metadata-audit.rs ||
  fail "compiled provider metadata does not enforce proof-first adapter ordering"
for falsifier in \
  missing_adapter_metadata_positive_control_is_rejected \
  missing_compatibility_metadata_positive_control_is_rejected \
  adapter_return_requires_proof_at_input_zero \
  adapter_input_is_refused \
  core_authority_type_is_refused \
  prepared_connection_constructor_is_refused \
  duplicate_proof_constructor_is_refused \
  exact_compiled_surface_passes_the_full_audit; do
  grep -qF "fn $falsifier" crates/nmp-nip46-ffi/metadata-audit.rs ||
    fail "compiled metadata audit is missing falsifier $falsifier"
done
grep -qF -- '--bin "$METADATA_AUDIT_BIN"' scripts/build-component-release.sh ||
  fail "managed provider builds do not audit compiled UniFFI metadata before packaging"
grep -qF 'packagedInterfaceIdentity: nmpProviderComponentInterfaceIdentity()' \
  Packages/NMPNip46/Sources/NMPNip46/RemoteSigner.swift ||
  fail "Swift provider does not verify packaged interface before preparing an adapter"
grep -qF 'nmpProviderComponentInterfaceIdentity(),' \
  Packages/NMPKotlin/nip46/src/main/kotlin/com/nmp/sdk/RemoteSigner.kt ||
  fail "Kotlin provider does not verify packaged interface before preparing an adapter"
for builder in scripts/build-swift-xcframework.sh scripts/build-kotlin-jvm.sh; do
  grep -qF 'scripts/build-component-release.sh' "$builder" ||
    fail "$builder does not consume sealed component snapshots"
  if grep -qF 'NMP_COMPONENT_BUILD_AUTH=' "$builder"; then
    fail "$builder can still carry component authorization outside the managed Cargo invocation"
  fi
done
grep -qF 'cargo build --frozen -p "$CARGO_PACKAGE" --release --target "$TARGET"' \
  scripts/build-component-release.sh ||
  fail "managed component build does not freeze one exact package root"
grep -qF 'scripts/verify-component-manifests.py' scripts/build-component-release.sh ||
  fail "managed component build does not verify its exact manifest set"
grep -qF 'rm -f "$AUTHORIZATION"' scripts/build-component-release.sh ||
  fail "managed component build does not revoke its authorization before returning"
grep -qF 'nmp-component-artifacts' scripts/build-component-release.sh ||
  fail "managed component build does not seal package inputs outside the reusable Cargo target"
if grep -nE 'Arc<NmpEngine>|engine:[[:space:]]*Arc<nmp::Engine>|FfiSigning(Capability)?Callback|SigningCapabilityCallback' \
  crates/nmp-nip46-ffi/src/signer.rs; then
  fail "NIP-46 FFI bypasses the opaque adapter or recreates a callback bridge"
fi

# Mutation controls for the two repeatedly-restored bad designs.
legacy_mailbox_mutation='pub unsafe fn assemble_core_signer_mailbox() -> FfiSignerMailbox'
ambient_runtime_mutation='tokio::runtime::Handle::current(); tokio::spawn(async {})'
printf '%s\n' "$legacy_mailbox_mutation" |
  grep -qE 'pub unsafe|[Mm]ailbox' ||
  fail "legacy mailbox mutation positive control escaped"
printf '%s\n' "$ambient_runtime_mutation" |
  grep -qE 'Handle::current[[:space:]]*\(|tokio::spawn[[:space:]]*\(' ||
  fail "ambient runtime mutation positive control escaped"

# Provider-only source must stay physically outside both core native roots.
if find Packages/NMP/Sources/NMP Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk \
  -type f \( -iname '*nip46*' -o -iname '*remote*signer*' \) | grep -q .; then
  fail "a NIP-46 native source file is still owned by a core package"
fi

echo "nip46-provider-boundary: ok"
