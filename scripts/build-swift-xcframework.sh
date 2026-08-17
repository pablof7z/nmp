#!/usr/bin/env bash
# Build the generated Swift bindings and NMP.xcframework from nmp-ffi.
#
# Compilation is Bazel; lipo/uniffi-bindgen/xcodebuild are the Apple and UniFFI
# packaging steps that turn the compiled slices into a distributable framework.
# No Cargo invocation remains in this path.
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
Each slice is a Bazel build under its own platform; Bazel supplies the Rust
standard library for every target triple, so nothing has to be installed
first.
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
# No CFLAGS/CXXFLAGS plumbing: those existed to push the macOS minimum into
# Cargo's C build scripts. Bazel passes it to the cc toolchain with
# --macos_minimum_os and to rustc with extra_rustc_env (.bazelrc).

# Bazel downloads the Rust standard library for every target triple it is
# configured for (MODULE.bazel `extra_target_triples`), so there is no rustup
# step here any more and no dependency on the host toolchain having the targets
# installed.

STAGE="$REPO_ROOT/target/xcframework-slices"
rm -rf "$STAGE"

# One Bazel build per platform. `-c opt` is the `--release` the Cargo build
# used; the feature set is not passed here because the staticlib target IS the
# all-features artifact (crates/nmp-ffi/BUILD.bazel).
build_slice() {  # <bazelrc config> <slice dir name>  -> prints the slice dir
  bazel build -c opt --config="$1" //crates/nmp-ffi:nmp_ffi_static 1>&2
  mkdir -p "$STAGE/$2"
  # Bazel outputs are read-only, so replace rather than overwrite.
  rm -f "$STAGE/$2/$LIB_NAME"
  cp "$REPO_ROOT/bazel-bin/crates/nmp-ffi/$LIB_NAME" "$STAGE/$2/$LIB_NAME"
  chmod u+w "$STAGE/$2/$LIB_NAME"
  printf '%s\n' "$STAGE/$2"
}

# The macOS minimum lives in Package.swift and again in .bazelrc, because
# rustc reads it from its own environment. Two spellings of one number is a
# mirror, so check them rather than trusting them.
BAZELRC_MACOS_MIN=$(sed -nE 's/^build:macos-arm64 .*MACOSX_DEPLOYMENT_TARGET=([0-9.]+).*/\1/p' "$REPO_ROOT/.bazelrc")
if [[ "$BAZELRC_MACOS_MIN" != "$MACOS_DEPLOYMENT_TARGET" ]]; then
  echo "error: macOS minimum disagrees: Package.swift says $MACOS_DEPLOYMENT_TARGET, .bazelrc says ${BAZELRC_MACOS_MIN:-<unset>}" >&2
  exit 1
fi

echo "== 1. bazel build (opt) =="
if [[ "$MODE" != macos ]]; then
  SIM_ARM_RELEASE_DIR=$(build_slice ios-sim-arm64 ios-sim-arm64)
  SIM_X86_RELEASE_DIR=$(build_slice ios-sim-x86_64 ios-sim-x86_64)
fi
MACOS_RELEASE_DIR=$(build_slice macos-arm64 macos-arm64)
if [[ "$MODE" == all ]]; then
  DEVICE_RELEASE_DIR=$(build_slice ios-device ios-device)
fi

SIM_ARM_LIB="${SIM_ARM_RELEASE_DIR:-}/$LIB_NAME"
SIM_X86_LIB="${SIM_X86_RELEASE_DIR:-}/$LIB_NAME"
MACOS_LIB="$MACOS_RELEASE_DIR/$LIB_NAME"
DEVICE_LIB="${DEVICE_RELEASE_DIR:-}/$LIB_NAME"

if [[ "$MODE" != macos ]]; then
  echo "== 2. lipo the two simulator arches into one fat staticlib =="
  FAT_SIM_DIR="$STAGE/ios-sim-fat"
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
# `bazel run` starts in the runfiles tree, so every path handed to the tool
# must be absolute.
bazel run -c opt "//crates/nmp-ffi:$BINDGEN_BIN" -- generate \
  --library "$BINDGEN_LIB" \
  --language swift \
  --out-dir "$GEN_DIR"

# The xcframework carries only the generated C header and modulemap. The
# generated Swift source remains an ordinary NMPFFI target source.
HEADERS_DIR="$STAGE/ios-ffi-headers"
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
echo "Bazel slice staging:       $STAGE"
echo "Raw bindgen output:        $GEN_DIR/"
echo "xcframework:               $XCFRAMEWORK_OUT ($SLICES)"
echo "Swift bindings source:     $SWIFT_SOURCE"
