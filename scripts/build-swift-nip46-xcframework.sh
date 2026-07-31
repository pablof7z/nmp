#!/usr/bin/env bash
# Build the separately selectable NIP-46 Swift component against each
# the exact independently sealed standalone-core static artifact.

set -euo pipefail

REPO_ROOT=$(git rev-parse --show-toplevel)
NMP_SWIFT_PAIRED_NIP46=1 \
  "$REPO_ROOT/scripts/build-swift-xcframework.sh" "$@"
