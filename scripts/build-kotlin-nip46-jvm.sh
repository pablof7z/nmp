#!/usr/bin/env bash
# Build the separately selectable NIP-46 Kotlin/JVM component. The generated
# provider bindings import the core mailbox converter from the root module.

set -euo pipefail

REPO_ROOT=$(git rev-parse --show-toplevel)
COMPONENT_PACKAGES="nmp-ffi nmp-nip46-ffi"

# Refresh the core JNA library and bindings from the same Cargo resolution as
# the provider before packaging the external mailbox consumer.
NMP_FFI_CARGO_PACKAGES="$COMPONENT_PACKAGES" \
  "$REPO_ROOT/scripts/build-kotlin-jvm.sh" "$@"

NMP_FFI_CARGO_PACKAGES="$COMPONENT_PACKAGES" \
NMP_FFI_CRATE=nmp-nip46-ffi \
NMP_FFI_LIB_STEM=nmp_nip46_ffi \
NMP_UNIFFI_BINDGEN_BIN=nmp-nip46-uniffi-bindgen \
NMP_KOTLIN_GEN_DIR=gen-kotlin-nip46 \
NMP_KOTLIN_MODULE_DIR=Packages/NMPKotlin/nip46 \
  "$REPO_ROOT/scripts/build-kotlin-jvm.sh" "$@"
