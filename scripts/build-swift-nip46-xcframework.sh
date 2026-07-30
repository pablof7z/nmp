#!/usr/bin/env bash
# Build the separately selectable NIP-46 Swift component together with its
# matched core. One managed Cargo build per selected target supplies both
# XCFrameworks, so core is never recompiled or repackaged in a second pass.

set -euo pipefail

REPO_ROOT=$(git rev-parse --show-toplevel)
COMPONENT_PACKAGES="nmp-ffi nmp-nip46-ffi"

NMP_FFI_CARGO_PACKAGES="$COMPONENT_PACKAGES" \
NMP_SWIFT_PAIRED_NIP46=1 \
  "$REPO_ROOT/scripts/build-swift-xcframework.sh" "$@"
