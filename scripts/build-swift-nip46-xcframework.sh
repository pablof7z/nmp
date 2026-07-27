#!/usr/bin/env bash
# Build the separately selectable NIP-46 Swift component. All target/mode
# handling remains in the core XCFramework builder so the two artifacts use
# identical slices and deployment targets.

set -euo pipefail

REPO_ROOT=$(git rev-parse --show-toplevel)

NMP_FFI_CRATE=nmp-nip46-ffi \
NMP_FFI_LIB_STEM=nmp_nip46_ffi \
NMP_UNIFFI_BINDGEN_BIN=nmp-nip46-uniffi-bindgen \
NMP_SWIFT_GEN_DIR="$REPO_ROOT/gen-nip46" \
NMP_SWIFT_PACKAGE_DIR="$REPO_ROOT/Packages/NMPNip46" \
NMP_SWIFT_FFI_TARGET=NMPNip46FFI \
NMP_SWIFT_XCFRAMEWORK_NAME=NMPNip46.xcframework \
NMP_SWIFT_EXTERNAL_MODULE=NMPFFI \
  "$REPO_ROOT/scripts/build-swift-xcframework.sh" "$@"
