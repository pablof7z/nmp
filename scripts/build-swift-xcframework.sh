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

CRATE=nmp-ffi
LIB_NAME=libnmp_ffi.a
GEN_DIR="$REPO_ROOT/gen"
XCFRAMEWORK_OUT="$REPO_ROOT/Packages/NMP/NMP.xcframework"

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
# script runs Cargo at the repository root, so resolve the same path here when
# locating the resulting archives and storing packaging intermediates.
TARGET_DIR_VALUE=${CARGO_TARGET_DIR:-target}
if [[ "$TARGET_DIR_VALUE" == /* ]]; then
  TARGET_DIR="$TARGET_DIR_VALUE"
else
  TARGET_DIR="$REPO_ROOT/$TARGET_DIR_VALUE"
fi

echo "== 1. cargo build (release) =="
if [[ "$MODE" != macos ]]; then
  env -u MACOSX_DEPLOYMENT_TARGET \
    cargo build -p "$CRATE" --release --target "$SIM_ARM_TARGET"
  env -u MACOSX_DEPLOYMENT_TARGET \
    cargo build -p "$CRATE" --release --target "$SIM_X86_TARGET"
fi
MACOSX_DEPLOYMENT_TARGET="$MACOS_DEPLOYMENT_TARGET" \
  CFLAGS="$MACOS_CFLAGS" \
  CXXFLAGS="$MACOS_CXXFLAGS" \
  cargo build -p "$CRATE" --release --target "$MACOS_TARGET"
if [[ "$MODE" == all ]]; then
  env -u MACOSX_DEPLOYMENT_TARGET \
    cargo build -p "$CRATE" --release --target "$DEVICE_TARGET"
fi

SIM_ARM_LIB="$TARGET_DIR/$SIM_ARM_TARGET/release/$LIB_NAME"
SIM_X86_LIB="$TARGET_DIR/$SIM_X86_TARGET/release/$LIB_NAME"
MACOS_LIB="$TARGET_DIR/$MACOS_TARGET/release/$LIB_NAME"
DEVICE_LIB="$TARGET_DIR/$DEVICE_TARGET/release/$LIB_NAME"

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
  cargo run -p "$CRATE" --bin uniffi-bindgen -- generate \
  --library "$BINDGEN_LIB" \
  --language swift \
  --out-dir "$GEN_DIR"

# The xcframework carries only the generated C header and modulemap. The
# generated Swift source remains an ordinary NMPFFI target source.
HEADERS_DIR="$TARGET_DIR/ios-ffi-headers"
rm -rf "$HEADERS_DIR"
mkdir -p "$HEADERS_DIR"
cp "$GEN_DIR/nmp_ffiFFI.h" "$HEADERS_DIR/"
cp "$GEN_DIR/nmp_ffiFFI.modulemap" "$HEADERS_DIR/module.modulemap"

SWIFT_SOURCES_DIR="$REPO_ROOT/Packages/NMP/Sources/NMPFFI"
mkdir -p "$SWIFT_SOURCES_DIR"
cp "$GEN_DIR/nmp_ffi.swift" "$SWIFT_SOURCES_DIR/"

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
echo "Swift bindings source:     $SWIFT_SOURCES_DIR/nmp_ffi.swift"
