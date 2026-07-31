#!/usr/bin/env bash
# Build the generated Swift bindings and NMP.xcframework from nmp-ffi.
#
# The default keeps the historical device + simulator + macOS output.
# `--sim-only` keeps the historical simulator + macOS CI output, while
# `--macos-only` prepares only the host artifact needed by SwiftPM builds and
# tests that do not run an iOS target.

set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: scripts/build-swift-xcframework.sh [OPTION]

Build generated Swift bindings and NMP.xcframework from nmp-ffi.

Options:
  --sim-only    build iOS simulator and macOS slices, but no device slice
  --macos-only  build only the macOS slice needed by host SwiftPM builds
  -h, --help    show this help without building

With no option, build iOS device, iOS simulator, and macOS slices.
CARGO_TARGET_DIR is honored when supplied by the caller.
USAGE
}

fail_usage() {
  echo "error: $1" >&2
  usage >&2
  exit 2
}

MODE=all
SHOW_HELP=0
for arg in "$@"; do
  case "$arg" in
    --sim-only)
      [[ "$MODE" == all || "$MODE" == sim ]] \
        || fail_usage "--sim-only and --macos-only cannot be combined"
      MODE=sim
      ;;
    --macos-only)
      [[ "$MODE" == all || "$MODE" == macos ]] \
        || fail_usage "--sim-only and --macos-only cannot be combined"
      MODE=macos
      ;;
    -h|--help)
      SHOW_HELP=1
      ;;
    --*)
      fail_usage "unknown option: $arg"
      ;;
    *)
      fail_usage "unexpected argument: $arg"
      ;;
  esac
done

if [[ "$SHOW_HELP" -eq 1 ]]; then
  usage
  exit 0
fi

REPO_ROOT=$(git rev-parse --show-toplevel)
cd "$REPO_ROOT"

CRATE=${NMP_FFI_CRATE:-nmp-ffi}
LIB_STEM=${NMP_FFI_LIB_STEM:-nmp_ffi}
BINDGEN_BIN=${NMP_UNIFFI_BINDGEN_BIN:-uniffi-bindgen}
GEN_DIR=${NMP_SWIFT_GEN_DIR:-"$REPO_ROOT/gen"}
SWIFT_PACKAGE_DIR=${NMP_SWIFT_PACKAGE_DIR:-"$REPO_ROOT/Packages/NMP"}
SWIFT_FFI_TARGET=${NMP_SWIFT_FFI_TARGET:-NMPFFI}
XCFRAMEWORK_NAME=${NMP_SWIFT_XCFRAMEWORK_NAME:-NMP.xcframework}
EXTERNAL_SWIFT_MODULE=${NMP_SWIFT_EXTERNAL_MODULE:-}
LIB_NAME="lib$LIB_STEM.a"
XCFRAMEWORK_OUT="$SWIFT_PACKAGE_DIR/$XCFRAMEWORK_NAME"

DEVICE_TARGET=aarch64-apple-ios
SIM_ARM_TARGET=aarch64-apple-ios-sim
SIM_X86_TARGET=x86_64-apple-ios
MACOS_TARGET=aarch64-apple-darwin
DEPLOYMENT_CHECKER="$REPO_ROOT/scripts/check-macos-deployment-target.sh"
MACOS_DEPLOYMENT_TARGET=$(
  "$DEPLOYMENT_CHECKER" --print-deployment-target
)
# Some native build scripts fingerprint CFLAGS but not MACOSX_DEPLOYMENT_TARGET.
# Set both so cached C/C++ objects are rebuilt when the package minimum changes.
MACOS_CFLAGS="${CFLAGS:+$CFLAGS }-mmacosx-version-min=$MACOS_DEPLOYMENT_TARGET"
MACOS_CXXFLAGS="${CXXFLAGS:+$CXXFLAGS }-mmacosx-version-min=$MACOS_DEPLOYMENT_TARGET"

# Cargo resolves a relative CARGO_TARGET_DIR from its working directory. The
# script runs Cargo at the repository root, so make it absolute first.
TARGET_DIR_VALUE=${CARGO_TARGET_DIR:-target}
if [[ "$TARGET_DIR_VALUE" == /* ]]; then
  TARGET_DIR="$TARGET_DIR_VALUE"
else
  TARGET_DIR="$REPO_ROOT/$TARGET_DIR_VALUE"
fi

build_target() {
  CARGO_TARGET_DIR="$TARGET_DIR" \
    cargo build --frozen -p "$CRATE" --release --target "$1" 1>&2
  printf '%s\n' "$TARGET_DIR/$1/release"
}

build_target_without_macos() {
  env -u MACOSX_DEPLOYMENT_TARGET bash -c \
    'CARGO_TARGET_DIR="$1" cargo build --frozen -p "$2" --release --target "$3" 1>&2' \
    _ "$TARGET_DIR" "$CRATE" "$1"
  printf '%s\n' "$TARGET_DIR/$1/release"
}

echo "== 1. cargo build (release) =="
cargo fetch --locked
if [[ "$MODE" != macos ]]; then
  SIM_ARM_RELEASE_DIR=$(build_target_without_macos "$SIM_ARM_TARGET")
  SIM_X86_RELEASE_DIR=$(build_target_without_macos "$SIM_X86_TARGET")
fi
MACOS_RELEASE_DIR=$(
  MACOSX_DEPLOYMENT_TARGET="$MACOS_DEPLOYMENT_TARGET" \
  CFLAGS="$MACOS_CFLAGS" \
  CXXFLAGS="$MACOS_CXXFLAGS" \
    build_target "$MACOS_TARGET"
)
if [[ "$MODE" == all ]]; then
  DEVICE_RELEASE_DIR=$(build_target_without_macos "$DEVICE_TARGET")
fi

SIM_ARM_LIB="${SIM_ARM_RELEASE_DIR:-}/$LIB_NAME"
SIM_X86_LIB="${SIM_X86_RELEASE_DIR:-}/$LIB_NAME"
MACOS_LIB="$MACOS_RELEASE_DIR/$LIB_NAME"
DEVICE_LIB="${DEVICE_RELEASE_DIR:-}/$LIB_NAME"

echo "== 1b. verify macOS deployment target ($MACOS_DEPLOYMENT_TARGET) =="
"$DEPLOYMENT_CHECKER" "$MACOS_LIB"

if [[ "$MODE" != macos ]]; then
  echo "== 2. lipo the two simulator arches into one fat staticlib =="
  FAT_SIM_DIR="$TARGET_DIR/ios-sim-fat"
  mkdir -p "$FAT_SIM_DIR"
  FAT_SIM_LIB="$FAT_SIM_DIR/$LIB_NAME"
  lipo -create "$SIM_ARM_LIB" "$SIM_X86_LIB" -output "$FAT_SIM_LIB"
  lipo -info "$FAT_SIM_LIB"
  BINDGEN_LIB="$SIM_ARM_LIB"
else
  echo "== 2. simulator lipo skipped (macOS only) =="
  BINDGEN_LIB="$MACOS_LIB"
fi

echo "== 3. uniffi-bindgen (library mode) -> Swift bindings =="
mkdir -p "$GEN_DIR"
env -u MACOSX_DEPLOYMENT_TARGET \
  cargo run --locked -p "$CRATE" --bin "$BINDGEN_BIN" -- generate \
  --library "$BINDGEN_LIB" \
  --language swift \
  --out-dir "$GEN_DIR"

# The xcframework carries only the generated C header and modulemap. The
# generated Swift source remains an ordinary NMPFFI target source.
HEADERS_DIR="$TARGET_DIR/ios-ffi-headers"
rm -rf "$HEADERS_DIR"
mkdir -p "$HEADERS_DIR"
cp "$GEN_DIR/${LIB_STEM}FFI.h" "$HEADERS_DIR/"
cp "$GEN_DIR/${LIB_STEM}FFI.modulemap" "$HEADERS_DIR/module.modulemap"

SWIFT_SOURCES_DIR="$SWIFT_PACKAGE_DIR/Sources/$SWIFT_FFI_TARGET"
mkdir -p "$SWIFT_SOURCES_DIR"
SWIFT_SOURCE="$SWIFT_SOURCES_DIR/$LIB_STEM.swift"
if [[ -n "$EXTERNAL_SWIFT_MODULE" ]]; then
  GENERATED_SOURCE="$GEN_DIR/$LIB_STEM.swift"
  TEMP_SOURCE="$SWIFT_SOURCE.tmp"
  awk -v module="$EXTERNAL_SWIFT_MODULE" '
    { print }
    $0 == "import Foundation" { print "import " module }
  ' "$GENERATED_SOURCE" > "$TEMP_SOURCE"
  mv "$TEMP_SOURCE" "$SWIFT_SOURCE"
else
  cp "$GEN_DIR/$LIB_STEM.swift" "$SWIFT_SOURCE"
fi

echo "== 4. xcodebuild -create-xcframework =="
mkdir -p "$(dirname "$XCFRAMEWORK_OUT")"
rm -rf "$XCFRAMEWORK_OUT"

XCFRAMEWORK_ARGS=(-library "$MACOS_LIB" -headers "$HEADERS_DIR")
SLICES=macos-arm64
if [[ "$MODE" != macos ]]; then
  XCFRAMEWORK_ARGS=(
    -library "$FAT_SIM_LIB" -headers "$HEADERS_DIR"
    "${XCFRAMEWORK_ARGS[@]}"
  )
  SLICES="ios-simulator + $SLICES"
fi
if [[ "$MODE" == all ]]; then
  XCFRAMEWORK_ARGS=(
    -library "$DEVICE_LIB" -headers "$HEADERS_DIR"
    "${XCFRAMEWORK_ARGS[@]}"
  )
  SLICES="ios-device + $SLICES"
fi

xcodebuild -create-xcframework \
  "${XCFRAMEWORK_ARGS[@]}" \
  -output "$XCFRAMEWORK_OUT"

echo "== done =="
echo "Cargo target directory:    $TARGET_DIR"
echo "Raw bindgen output:        $GEN_DIR/"
echo "xcframework:               $XCFRAMEWORK_OUT ($SLICES)"
echo "Swift bindings source:     $SWIFT_SOURCE"
