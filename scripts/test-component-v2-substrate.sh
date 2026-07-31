#!/usr/bin/env bash
# #952 structural falsifier for the core-anchored native component substrate.

set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

fail() {
  echo "component-v2-substrate: $*" >&2
  exit 1
}

[[ -f crates/nmp-component-interface/Cargo.toml ]] ||
  fail "the shared component interface package is missing"
[[ -f scripts/verify-component-manifests.py ]] ||
  fail "the exact component manifest-set verifier is missing"

if grep -qF 'nip46-provider-component' crates/nmp-ffi/Cargo.toml; then
  fail "the package-set-specific core feature still exists"
fi
if sed -n '/^\[dependencies\]/,/^\[/p' crates/nmp-nip46-ffi/Cargo.toml |
  grep -Eq '^(nmp|nmp-ffi)[[:space:]]*='; then
  fail "the optional NIP-46 artifact still links the core facade"
fi

grep -qF 'nmp-component-interface' crates/nmp-nip46-ffi/Cargo.toml ||
  fail "the optional NIP-46 artifact does not use the shared interface"
interface_source=crates/nmp-component-interface/src/signer.rs
core_source=crates/nmp-ffi/src/signer.rs
provider_source=crates/nmp-nip46-ffi/src/signer.rs

grep -qF 'pub struct FfiSignerAdapter' "$interface_source" ||
  fail "the shared take-once signer adapter is missing"
grep -qF 'pub fn new_signer_adapter(' "$interface_source" ||
  fail "the provider cannot prepare an opaque signer adapter"
grep -qF 'pub struct SignerAdapterRuntime' "$interface_source" ||
  fail "the core-minted contextual runtime capability is missing"
grep -qF 'let _entered = handle.enter();' "$interface_source" ||
  fail "provider futures are not entered in their linked Tokio context on every poll"
if grep -qE 'pub fn from_core|pub unsafe|core-owner|[Mm]ailbox|CoreSigner(Port|Lease)' \
  "$interface_source" "$core_source" "$provider_source"; then
  fail "deleted unsafe/mailbox authority or a public runtime minting door survives"
fi
[[ $(awk '{ total += gsub(/\.take_for_install\(\)/, "") } END { print total + 0 }' \
  "$core_source") == 1 ]] ||
  fail "the core must consume the provider adapter at exactly one installation site"
grep -qF 'pub(crate) fn install_signer_adapter(' "$core_source" ||
  fail "the core-owned adapter installation door is missing"
if grep -qE 'Handle::current[[:space:]]*\(|tokio::spawn[[:space:]]*\(|runtime::Builder::new' \
  "$provider_source"; then
  fail "the separately linked provider regained ambient or provider-owned runtime authority"
fi
grep -qF 'Arc<dyn nmp_signer::Nip46TaskRuntime>' "$provider_source" ||
  fail "provider child tasks do not use the core-minted contextual scheduler"

# Positive controls: each deleted design, if restored, must match the exact
# structural refusal above instead of relying on reviewer memory.
legacy_mailbox_mutation='pub unsafe fn assemble_core_signer_mailbox() -> FfiSignerMailbox'
ambient_runtime_mutation='let runtime = tokio::runtime::Handle::current(); tokio::spawn(async {})'
printf '%s\n' "$legacy_mailbox_mutation" |
  grep -qE 'pub unsafe|[Mm]ailbox' ||
  fail "legacy mailbox mutation positive control escaped"
printf '%s\n' "$ambient_runtime_mutation" |
  grep -qE 'Handle::current[[:space:]]*\(|tokio::spawn[[:space:]]*\(' ||
  fail "ambient runtime mutation positive control escaped"
grep -qF 'nmp-core-component-v2' crates/nmp-ffi/build.rs ||
  fail "the core identity is not v2"

for manifest in crates/nmp-ffi/Cargo.toml crates/nmp-nip46-ffi/Cargo.toml; do
  grep -qF '[package.metadata.nmp-component]' "$manifest" ||
    fail "$manifest has no generic component metadata"
done

if grep -qE 'PACKAGE_SET|PROVIDER_ONLY_CRATES|nmp-core-component-v1' \
  scripts/build-component-release.sh crates/nmp-ffi/build.rs; then
  fail "the v1 pair/package-set identity path still survives"
fi

echo "component-v2-substrate: take-once adapter and contextual core runtime present"
