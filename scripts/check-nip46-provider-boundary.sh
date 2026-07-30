#!/usr/bin/env bash
# #877/#952: NIP-46 is a selectable signer provider, never part of the
# universal signer/core/native vocabulary. A provider must also prove exact
# native-core compatibility before it may receive the external mailbox.

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
  grep -qF 'find Packages/NMPNip46/NMPNip46.xcframework' "$swift_provider_workflow" ||
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
}

check_provider_workflows
if [[ "$MODE" == workflows ]]; then
  echo "nip46-provider-boundary: provider workflow ownership ok"
  exit 0
fi

required_paths=(
  crates/nmp-signer/src/capability.rs
  crates/nmp-local-signer/src/local.rs
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

# The provider consumes the core component's one opaque mailbox. Importing the
# engine object, mirroring an engine, or adding a foreign signing callback
# would create another lifecycle/boundary and is forbidden by the issue.
grep -qF 'pub struct FfiSignerMailbox' crates/nmp-ffi/src/signer.rs ||
  fail "core opaque signer mailbox is missing"
grep -qF 'pub fn signer_mailbox(&self) -> Arc<FfiSignerMailbox>' crates/nmp-ffi/src/facade.rs ||
  fail "NmpEngine does not vend the opaque signer mailbox"
grep -qF 'pub(crate) fn from_engine(engine: Arc<nmp::Engine>) -> Arc<Self>' \
  crates/nmp-ffi/src/signer.rs ||
  fail "opaque signer mailbox construction is not sealed inside core"
grep -qF 'pub fn nmp_core_component_identity() -> String' crates/nmp-ffi/src/signer.rs ||
  fail "core does not export its plain native component identity"
grep -qF '"--unit-graph"' crates/nmp-ffi/build.rs ||
  fail "core identity does not derive Cargo's resolved transitive unit graph"
grep -qF 'validate_unit_graph_against_cargo' crates/nmp-ffi/build.rs ||
  fail "core identity does not validate its derived graph against Cargo's resolved marker"
grep -qF 'validated_release_marker' crates/nmp-ffi/build.rs ||
  fail "core release identity is not bound to an isolated component target"
grep -qF 'NMP_FFI_COMPONENT_AUTH' crates/nmp-ffi/build.rs ||
  fail "isolated component target lacks per-build builder authorization"
grep -qF 'features = ["nip46-provider-component"]' crates/nmp-nip46-ffi/Cargo.toml ||
  fail "NIP-46 provider does not make its presence observable to the nmp-ffi build"
grep -qF 'pub fn verify_nip46_core_component_identity(' crates/nmp-nip46-ffi/src/signer.rs ||
  fail "NIP-46 provider does not verify plain core identity before object exchange"
grep -qF 'compatibility: Arc<FfiNip46CoreCompatibility>' crates/nmp-nip46-ffi/src/signer.rs ||
  fail "NIP-46 provider construction does not require a compatibility proof"
grep -qF 'macro_metadata::extract_from_library' crates/nmp-nip46-ffi/metadata-audit.rs ||
  fail "mailbox-entry falsifier does not inspect UniFFI's compiled export authority"
grep -qF 'const CORE_MAILBOX_SOURCE: &str = "nmp_ffi::NmpEngine::signer_mailbox";' \
  crates/nmp-nip46-ffi/metadata-audit.rs ||
  fail "compiled metadata audit does not pin the one outward-only core mailbox source"
grep -qF 'core_mailbox_sources != 1' crates/nmp-nip46-ffi/metadata-audit.rs ||
  fail "compiled metadata audit does not require exactly one core mailbox source"
grep -qF 'compiled UniFFI metadata must expose exactly one proof-bearing mailbox entry' \
  crates/nmp-nip46-ffi/metadata-audit.rs ||
  fail "compiled provider metadata does not enforce the single proof-bearing mailbox entry"
for falsifier in \
  exact_compiled_constructor_is_the_only_mailbox_entry \
  missing_compiled_mailbox_entry_is_rejected \
  exact_compiled_constructor_name_is_required \
  missing_mailbox_metadata_positive_control_is_rejected \
  missing_compatibility_metadata_positive_control_is_rejected \
  missing_core_mailbox_source_positive_control_is_rejected \
  foreign_namespace_mailbox_entry_is_not_hidden_from_audit \
  forged_core_namespace_mailbox_input_is_not_exempted \
  exact_core_mailbox_source_cannot_accept_a_mailbox_input \
  exact_core_mailbox_source_cannot_throw_a_mailbox \
  duplicate_core_mailbox_source_is_rejected; do
  grep -qF "fn $falsifier" crates/nmp-nip46-ffi/metadata-audit.rs ||
    fail "compiled metadata audit is missing falsifier $falsifier"
done
grep -qF -- '--bin nmp-nip46-metadata-audit' scripts/build-component-release.sh ||
  fail "managed provider builds do not audit compiled UniFFI metadata before packaging"
grep -qF 'withVerifiedNip46Core(actual: nmpProviderCoreComponentIdentity())' \
  Packages/NMPNip46/Sources/NMPNip46/RemoteSigner.swift ||
  fail "Swift provider does not verify the loaded core before requesting a mailbox"
grep -qF 'withVerifiedNip46Core(nmpProviderCoreComponentIdentity())' \
  Packages/NMPKotlin/nip46/src/main/kotlin/com/nmp/sdk/RemoteSigner.kt ||
  fail "Kotlin provider does not verify the loaded core before requesting a mailbox"
for builder in scripts/build-swift-nip46-xcframework.sh scripts/build-kotlin-nip46-jvm.sh; do
  grep -qF 'NMP_FFI_CARGO_PACKAGES="$COMPONENT_PACKAGES"' "$builder" ||
    fail "$builder does not build core and provider under one package-set identity"
done
for builder in scripts/build-swift-xcframework.sh scripts/build-kotlin-jvm.sh; do
  grep -qF 'scripts/build-component-release.sh' "$builder" ||
    fail "$builder does not consume a sealed exact-package-set snapshot"
  if grep -qF 'NMP_FFI_COMPONENT_AUTH=' "$builder"; then
    fail "$builder can still carry component authorization outside the managed Cargo invocation"
  fi
  if grep -qF 'NMP_FFI_CARGO_UNIT_GRAPH' "$builder"; then
    fail "$builder still supplies declared graph content"
  fi
done
grep -qF 'cargo build --frozen "${PACKAGE_ARGS[@]}" --release --target "$TARGET"' \
  scripts/build-component-release.sh ||
  fail "managed component build does not freeze exact package roots, target, and release profile"
grep -qF 'rm -f "$AUTHORIZATION"' scripts/build-component-release.sh ||
  fail "managed component build does not revoke its authorization before returning"
grep -qF 'nmp-component-artifacts' scripts/build-component-release.sh ||
  fail "managed component build does not seal package inputs outside the reusable Cargo target"
if grep -qF 'NMP_FFI_CARGO_UNIT_GRAPH' crates/nmp-ffi/build.rs; then
  fail "build script still accepts caller-declared graph content"
fi
if grep -qF 'NMP_FFI_COMPONENT_BUILD' crates/nmp-ffi/build.rs; then
  fail "build script still trusts the obsolete broad enablement variable"
fi
if grep -nE 'Arc<NmpEngine>|engine:[[:space:]]*Arc<nmp::Engine>|FfiSigning(Capability)?Callback|SigningCapabilityCallback' \
  crates/nmp-nip46-ffi/src/signer.rs; then
  fail "NIP-46 FFI bypasses the opaque mailbox or recreates a callback bridge"
fi

# Provider-only source must stay physically outside both core native roots.
if find Packages/NMP/Sources/NMP Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk \
  -type f \( -iname '*nip46*' -o -iname '*remote*signer*' \) | grep -q .; then
  fail "a NIP-46 native source file is still owned by a core package"
fi

echo "nip46-provider-boundary: ok"
