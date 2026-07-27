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

# A provider staticlib and the core staticlib must be produced by one Cargo
# feature-resolution unit. Otherwise each archive can carry a differently
# hashed copy of the external core UniFFI object, and linking both components
# either duplicates every core C symbol or crosses incompatible Rust types.
# Core-only callers keep the one-package default; provider wrappers opt into
# the exact core + provider package set and refresh both artifacts.
read -r -a CARGO_PACKAGE_NAMES <<< "${NMP_FFI_CARGO_PACKAGES:-$CRATE}"
CARGO_PACKAGE_ARGS=()
for package_name in "${CARGO_PACKAGE_NAMES[@]}"; do
  CARGO_PACKAGE_ARGS+=(-p "$package_name")
done

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
# script runs Cargo at the repository root. Treat it as a BASE only: release
# artifacts live under a package-set-specific directory that ad-hoc Cargo
# builds never write.
TARGET_DIR_VALUE=${CARGO_TARGET_DIR:-target}
if [[ "$TARGET_DIR_VALUE" == /* ]]; then
  TARGET_DIR="$TARGET_DIR_VALUE"
else
  TARGET_DIR="$REPO_ROOT/$TARGET_DIR_VALUE"
fi

prepare_target() {
  "$REPO_ROOT/scripts/prepare-component-build.sh" \
    "$TARGET_DIR" "${CARGO_PACKAGE_NAMES[*]}" "$1"
}

echo "== 1. cargo build (isolated release) =="
cargo fetch --locked
if [[ "$MODE" != macos ]]; then
  SIM_ARM_COMPONENT_BUILD=$(prepare_target "$SIM_ARM_TARGET")
  SIM_ARM_COMPONENT_TARGET_DIR=${SIM_ARM_COMPONENT_BUILD%%$'\n'*}
  SIM_ARM_COMPONENT_AUTH=${SIM_ARM_COMPONENT_BUILD#*$'\n'}
  env -u MACOSX_DEPLOYMENT_TARGET \
    CARGO_TARGET_DIR="$SIM_ARM_COMPONENT_TARGET_DIR" \
    NMP_FFI_COMPONENT_AUTH="$SIM_ARM_COMPONENT_AUTH" \
    cargo build --frozen "${CARGO_PACKAGE_ARGS[@]}" --release --target "$SIM_ARM_TARGET"
  SIM_X86_COMPONENT_BUILD=$(prepare_target "$SIM_X86_TARGET")
  SIM_X86_COMPONENT_TARGET_DIR=${SIM_X86_COMPONENT_BUILD%%$'\n'*}
  SIM_X86_COMPONENT_AUTH=${SIM_X86_COMPONENT_BUILD#*$'\n'}
  env -u MACOSX_DEPLOYMENT_TARGET \
    CARGO_TARGET_DIR="$SIM_X86_COMPONENT_TARGET_DIR" \
    NMP_FFI_COMPONENT_AUTH="$SIM_X86_COMPONENT_AUTH" \
    cargo build --frozen "${CARGO_PACKAGE_ARGS[@]}" --release --target "$SIM_X86_TARGET"
fi
MACOS_COMPONENT_BUILD=$(prepare_target "$MACOS_TARGET")
MACOS_COMPONENT_TARGET_DIR=${MACOS_COMPONENT_BUILD%%$'\n'*}
MACOS_COMPONENT_AUTH=${MACOS_COMPONENT_BUILD#*$'\n'}
MACOSX_DEPLOYMENT_TARGET="$MACOS_DEPLOYMENT_TARGET" \
  CFLAGS="$MACOS_CFLAGS" \
  CXXFLAGS="$MACOS_CXXFLAGS" \
  CARGO_TARGET_DIR="$MACOS_COMPONENT_TARGET_DIR" \
  NMP_FFI_COMPONENT_AUTH="$MACOS_COMPONENT_AUTH" \
  cargo build --frozen "${CARGO_PACKAGE_ARGS[@]}" --release --target "$MACOS_TARGET"
if [[ "$MODE" == all ]]; then
  DEVICE_COMPONENT_BUILD=$(prepare_target "$DEVICE_TARGET")
  DEVICE_COMPONENT_TARGET_DIR=${DEVICE_COMPONENT_BUILD%%$'\n'*}
  DEVICE_COMPONENT_AUTH=${DEVICE_COMPONENT_BUILD#*$'\n'}
  env -u MACOSX_DEPLOYMENT_TARGET \
    CARGO_TARGET_DIR="$DEVICE_COMPONENT_TARGET_DIR" \
    NMP_FFI_COMPONENT_AUTH="$DEVICE_COMPONENT_AUTH" \
    cargo build --frozen "${CARGO_PACKAGE_ARGS[@]}" --release --target "$DEVICE_TARGET"
fi

COMPONENT_TARGET_DIR="$TARGET_DIR/nmp-component-build/$(
  if [[ "${CARGO_PACKAGE_NAMES[*]}" == "nmp-ffi" ]]; then
    printf core
  else
    printf nip46
  fi
)"
SIM_ARM_LIB="$COMPONENT_TARGET_DIR/$SIM_ARM_TARGET/release/$LIB_NAME"
SIM_X86_LIB="$COMPONENT_TARGET_DIR/$SIM_X86_TARGET/release/$LIB_NAME"
MACOS_LIB="$COMPONENT_TARGET_DIR/$MACOS_TARGET/release/$LIB_NAME"
DEVICE_LIB="$COMPONENT_TARGET_DIR/$DEVICE_TARGET/release/$LIB_NAME"

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
