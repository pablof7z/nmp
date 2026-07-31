#!/usr/bin/env bash
# Build the separately selectable NIP-46 Kotlin/JVM component. The generated
# provider bindings import the shared adapter contract from the root module.

set -euo pipefail

REPO_ROOT=$(git rev-parse --show-toplevel)
cd "$REPO_ROOT"

case "$(uname -s)" in
  Darwin) LIB_EXT=dylib ;;
  Linux) LIB_EXT=so ;;
  *)
    echo "error: unsupported OS $(uname -s)" >&2
    exit 1
    ;;
esac
TARGET_DIR_VALUE=${CARGO_TARGET_DIR:-target}
if [[ "$TARGET_DIR_VALUE" == /* ]]; then
  TARGET_DIR="$TARGET_DIR_VALUE"
else
  TARGET_DIR="$REPO_ROOT/$TARGET_DIR_VALUE"
fi
HOST_TARGET=$(rustc -vV | sed -n 's/^host: //p')
[[ -n "$HOST_TARGET" ]] || {
  echo "error: rustc -vV did not report a host target" >&2
  exit 1
}

# Build one sealed core snapshot. The root module packages these exact bytes,
# and the provider build consumes this same artifact plus its adjacent
# manifest/witness. No second core build can silently create a different pair.
CORE_COMPONENT_ARTIFACT_DIR=$(
  "$REPO_ROOT/scripts/build-component-release.sh" \
    "$TARGET_DIR" "$HOST_TARGET" nmp-ffi
)
cleanup_core_component_artifact() {
  if [[ -d "$CORE_COMPONENT_ARTIFACT_DIR" ]]; then
    chmod -R u+w "$CORE_COMPONENT_ARTIFACT_DIR" 2>/dev/null || true
    rm -r "$CORE_COMPONENT_ARTIFACT_DIR"
  fi
}
trap cleanup_core_component_artifact EXIT
CORE_COMPONENT_ARTIFACT="$CORE_COMPONENT_ARTIFACT_DIR/libnmp_ffi.$LIB_EXT"

NMP_COMPONENT_ARTIFACT_DIR="$CORE_COMPONENT_ARTIFACT_DIR" \
  "$REPO_ROOT/scripts/build-kotlin-jvm.sh" "$@"

case "$(uname -s)" in
  Darwin) JNA_OS=darwin ;;
  Linux) JNA_OS=linux ;;
esac
case "$(uname -m)" in
  arm64|aarch64) JNA_ARCH=aarch64 ;;
  x86_64) JNA_ARCH=x86-64 ;;
  *)
    echo "error: unsupported arch $(uname -m)" >&2
    exit 1
    ;;
esac
JNA_PREFIX="$JNA_OS-$JNA_ARCH"
PUBLISHED_CORE_COMPONENT_ARTIFACT="$REPO_ROOT/Packages/NMPKotlin/src/main/resources/$JNA_PREFIX/libnmp_ffi.$LIB_EXT"

NMP_FFI_CRATE=nmp-nip46-ffi \
NMP_FFI_LIB_STEM=nmp_nip46_ffi \
NMP_UNIFFI_BINDGEN_BIN=nmp-nip46-uniffi-bindgen \
NMP_KOTLIN_GEN_DIR=gen-kotlin-nip46 \
NMP_KOTLIN_MODULE_DIR=Packages/NMPKotlin/nip46 \
NMP_CORE_COMPONENT_ARTIFACT="$CORE_COMPONENT_ARTIFACT" \
NMP_PUBLISHED_CORE_COMPONENT_ARTIFACT="$PUBLISHED_CORE_COMPONENT_ARTIFACT" \
  "$REPO_ROOT/scripts/build-kotlin-jvm.sh" "$@"

"$REPO_ROOT/scripts/check-nip46-component-identity.sh" --matched-only \
  "$PUBLISHED_CORE_COMPONENT_ARTIFACT" \
  "$REPO_ROOT/Packages/NMPKotlin/nip46/src/main/resources/$JNA_PREFIX/libnmp_nip46_ffi.$LIB_EXT"
