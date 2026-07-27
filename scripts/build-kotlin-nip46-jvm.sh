#!/usr/bin/env bash
# Build the separately selectable NIP-46 Kotlin/JVM component. The generated
# provider bindings import the core mailbox converter from the root module.

set -euo pipefail

REPO_ROOT=$(git rev-parse --show-toplevel)

NMP_FFI_CRATE=nmp-nip46-ffi \
NMP_FFI_LIB_STEM=nmp_nip46_ffi \
NMP_UNIFFI_BINDGEN_BIN=nmp-nip46-uniffi-bindgen \
NMP_KOTLIN_GEN_DIR=gen-kotlin-nip46 \
NMP_KOTLIN_MODULE_DIR=Packages/NMPKotlin/nip46 \
  "$REPO_ROOT/scripts/build-kotlin-jvm.sh" "$@"
