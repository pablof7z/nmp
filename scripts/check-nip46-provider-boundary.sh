#!/usr/bin/env bash
# #877: NIP-46 is a selectable signer provider, never part of the universal
# signer/core/native vocabulary. This is the source-and-package half of the
# boundary falsifier; build jobs additionally run the core/provider tests and
# inspect their produced libraries.

set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

fail() { echo "nip46-provider-boundary: $*" >&2; exit 1; }

required_paths=(
  crates/nmp-signer/src/capability.rs
  crates/nmp-local-signer/src/local.rs
  crates/nmp-ffi/src/signer.rs
  crates/nmp-nip46/src/nip46.rs
  crates/nmp-nip46-ffi/src/signer.rs
  Packages/NMP/Sources/NMP/ProviderComponent.swift
  Packages/NMPNip46/Package.swift
  Packages/NMPNip46/Sources/NMPNip46/RemoteSigner.swift
  Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/ProviderComponent.kt
  Packages/NMPKotlin/nip46/build.gradle.kts
  Packages/NMPKotlin/nip46/src/main/kotlin/com/nmp/sdk/RemoteSigner.kt
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

# Core source and hand-written native SDKs cannot name the provider. Generated
# files and append-only history are outside this corpus by construction.
core_roots=(
  crates/nmp-signer/src
  crates/nmp-engine/src
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
grep -qF 'pub fn new(mailbox: Arc<FfiSignerMailbox>)' crates/nmp-nip46-ffi/src/signer.rs ||
  fail "NIP-46 native provider does not consume the external core mailbox"
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
