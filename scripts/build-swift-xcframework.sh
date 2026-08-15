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
Any Rust target the selected slices need is installed onto the toolchain
rust-toolchain.toml pins. CARGO_TARGET_DIR is honored when supplied by the
caller.
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

# NMP ships one native library, so these are constants rather than knobs.
CRATE=nmp-ffi
LIB_STEM=nmp_ffi
BINDGEN_BIN=uniffi-bindgen
GEN_DIR="$REPO_ROOT/gen"
SWIFT_PACKAGE_DIR="$REPO_ROOT/Packages/NMP"
SWIFT_FFI_TARGET=NMPFFI
LIB_NAME="lib$LIB_STEM.a"
XCFRAMEWORK_OUT="$SWIFT_PACKAGE_DIR/NMP.xcframework"

DEVICE_TARGET=aarch64-apple-ios
SIM_ARM_TARGET=aarch64-apple-ios-sim
SIM_X86_TARGET=x86_64-apple-ios
MACOS_TARGET=aarch64-apple-darwin
MACOS_DEPLOYMENT_TARGET=$(
  sed -nE 's/^[[:space:]]*\.macOS\(\.v([0-9]+)\),?[[:space:]]*$/\1.0/p' \
    "$SWIFT_PACKAGE_DIR/Package.swift"
)
if [[ -z "$MACOS_DEPLOYMENT_TARGET" ]]; then
  echo "error: no macOS deployment target found in $SWIFT_PACKAGE_DIR/Package.swift" >&2
  exit 1
fi
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

REQUIRED_TARGETS="$MACOS_TARGET"
if [[ "$MODE" != macos ]]; then
  REQUIRED_TARGETS="$REQUIRED_TARGETS $SIM_ARM_TARGET $SIM_X86_TARGET"
fi
if [[ "$MODE" == all ]]; then
  REQUIRED_TARGETS="$REQUIRED_TARGETS $DEVICE_TARGET"
fi

# A cross-compilation target's standard library is installed per toolchain, and
# this repository pins its toolchain in rust-toolchain.toml. This script is the
# only place that knows both the pin and the target set, so it installs the
# targets it is about to build for. Cargo without a target's std fails as
# `error[E0463]: can't find crate for core`, which reads like a source break in
# nmp-ffi and is not one.
echo "== 0. Rust targets for the toolchain this repository pins =="
if ! command -v rustup >/dev/null 2>&1; then
  echo "error: rustup is required: rust-toolchain.toml selects the toolchain this build must use" >&2
  echo "error: this build needs the Rust standard library for: $REQUIRED_TARGETS" >&2
  exit 1
fi
# `rustup target` acts on the ACTIVE toolchain, and the working directory is the
# repository root, so rust-toolchain.toml selects it. Installing against any
# other toolchain (`rustup +nightly target add ...`) leaves this build with no
# std for the target it names.
ACTIVE_TOOLCHAIN=$(rustup show active-toolchain)
ACTIVE_TOOLCHAIN=${ACTIVE_TOOLCHAIN%% *}
INSTALLED_TARGETS=$(rustup target list --installed)
MISSING_TARGETS=
for required_target in $REQUIRED_TARGETS; do
  grep -Fqx -- "$required_target" <<<"$INSTALLED_TARGETS" \
    || MISSING_TARGETS="${MISSING_TARGETS:+$MISSING_TARGETS }$required_target"
done
if [[ -n "$MISSING_TARGETS" ]]; then
  echo "installing on $ACTIVE_TOOLCHAIN: $MISSING_TARGETS"
  # shellcheck disable=SC2086 # target triples never contain whitespace
  if ! rustup target add $MISSING_TARGETS; then
    echo "error: no Rust standard library for: $MISSING_TARGETS" >&2
    echo "error: install it from the repository root, so rust-toolchain.toml selects the toolchain:" >&2
    echo "error:   rustup target add $MISSING_TARGETS" >&2
    echo "error: the active toolchain here is $ACTIVE_TOOLCHAIN; installing onto another one leaves cargo reporting a missing \`core\`" >&2
    exit 1
  fi
else
  echo "$ACTIVE_TOOLCHAIN already has: $REQUIRED_TARGETS"
fi

build_target() {
  CARGO_TARGET_DIR="$TARGET_DIR" \
    cargo build --frozen -p "$CRATE" --no-default-features --all-features --release --target "$1" 1>&2
  printf '%s\n' "$TARGET_DIR/$1/release"
}

build_target_without_macos() {
  env -u MACOSX_DEPLOYMENT_TARGET bash -c \
    'CARGO_TARGET_DIR="$1" cargo build --frozen -p "$2" --no-default-features --all-features --release --target "$3" 1>&2' \
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
  cargo run --locked -p "$CRATE" --bin "$BINDGEN_BIN" --no-default-features --all-features -- generate \
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
cp "$GEN_DIR/$LIB_STEM.swift" "$SWIFT_SOURCE"

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
