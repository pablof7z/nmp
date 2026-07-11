#!/usr/bin/env bash
# M4 plan §3: cargo -> xcframework -> (a later builder wires) SwiftPM.
#
# 1. Build the nmp-ffi staticlib for device + both simulator triples, plus a
#    macOS (aarch64-apple-darwin) slice.
# 2. lipo the two simulator arches into one fat sim staticlib (device stays
#    separate -- xcframework requires arch-disjoint slices).
# 3. Run uniffi-bindgen in LIBRARY mode against one compiled staticlib to
#    generate the Swift bindings (nmp_ffi.swift / nmp_ffiFFI.h / .modulemap)
#    -- no .udl file, metadata is read straight out of the compiled binary.
# 4. `xcodebuild -create-xcframework` the device + fat-sim + macOS slices
#    into NMP.xcframework.
#
# The macOS slice exists so `swift test` (which runs the package's own test
# host process on THIS Mac, not a simulator) can link NMPFFI at all --
# SwiftPM test hosts run on the build machine's own arch, so without a
# macos-arm64 slice `swift build`/`swift test` cannot resolve the
# binaryTarget. It carries the exact same Rust core as the iOS slices;
# nothing macOS-specific lives behind it.
#
# Usage: scripts/build-swift-xcframework.sh [--sim-only]
#   --sim-only  skip the device (aarch64-apple-ios) slice -- useful in CI/
#               sandboxes with no signing identity; the sim + macOS slices
#               alone prove the pipeline (M4 plan §C: "sim slice is enough
#               for now; device needs signing").

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

SIM_ONLY=0
if [[ "${1:-}" == "--sim-only" ]]; then
  SIM_ONLY=1
fi

CRATE=nmp-ffi
LIB_NAME=libnmp_ffi.a
GEN_DIR=gen
XCFRAMEWORK_OUT=Packages/NMP/NMP.xcframework

DEVICE_TARGET=aarch64-apple-ios
SIM_ARM_TARGET=aarch64-apple-ios-sim
SIM_X86_TARGET=x86_64-apple-ios
MACOS_TARGET=aarch64-apple-darwin

echo "== 1. cargo build (release) =="
cargo build -p "$CRATE" --release --target "$SIM_ARM_TARGET"
cargo build -p "$CRATE" --release --target "$SIM_X86_TARGET"
cargo build -p "$CRATE" --release --target "$MACOS_TARGET"
if [[ "$SIM_ONLY" -eq 0 ]]; then
  cargo build -p "$CRATE" --release --target "$DEVICE_TARGET"
fi

SIM_ARM_LIB="target/$SIM_ARM_TARGET/release/$LIB_NAME"
SIM_X86_LIB="target/$SIM_X86_TARGET/release/$LIB_NAME"
MACOS_LIB="target/$MACOS_TARGET/release/$LIB_NAME"
DEVICE_LIB="target/$DEVICE_TARGET/release/$LIB_NAME"

echo "== 2. lipo the two simulator arches into one fat staticlib =="
FAT_SIM_DIR="target/ios-sim-fat"
mkdir -p "$FAT_SIM_DIR"
FAT_SIM_LIB="$FAT_SIM_DIR/$LIB_NAME"
lipo -create "$SIM_ARM_LIB" "$SIM_X86_LIB" -output "$FAT_SIM_LIB"
lipo -info "$FAT_SIM_LIB"

echo "== 3. uniffi-bindgen (library mode) -> Swift bindings =="
mkdir -p "$GEN_DIR"
cargo run -p "$CRATE" --bin uniffi-bindgen -- generate \
  --library "$SIM_ARM_LIB" \
  --language swift \
  --out-dir "$GEN_DIR"

# Split bindgen's output: the xcframework's `-headers` slice should carry
# ONLY the C header + a `module.modulemap` (the conventional name Xcode's
# Clang importer looks for) -- never the generated `.swift` file, which
# belongs in the SwiftPM package's own `Sources/NMPFFI/` as a plain Swift
# source the "NMPFFI" target compiles (M4 plan §2's layout). This script
# stops at producing both pieces; wiring `Package.swift`'s two targets
# (`NMPFFI` binaryTarget + `NMP` ergonomic target) is the next builder's job
# (plan §8 step D/E).
HEADERS_DIR="target/ios-ffi-headers"
rm -rf "$HEADERS_DIR"
mkdir -p "$HEADERS_DIR"
cp "$GEN_DIR/nmp_ffiFFI.h" "$HEADERS_DIR/"
cp "$GEN_DIR/nmp_ffiFFI.modulemap" "$HEADERS_DIR/module.modulemap"

SWIFT_SOURCES_DIR="Packages/NMP/Sources/NMPFFI"
mkdir -p "$SWIFT_SOURCES_DIR"
cp "$GEN_DIR/nmp_ffi.swift" "$SWIFT_SOURCES_DIR/"

echo "== 4. xcodebuild -create-xcframework =="
mkdir -p "$(dirname "$XCFRAMEWORK_OUT")"
rm -rf "$XCFRAMEWORK_OUT"

# Headers/modulemap are arch-agnostic (same generated metadata for every
# slice) -- every -library shares the one $HEADERS_DIR.
XCFRAMEWORK_ARGS=(
  -library "$FAT_SIM_LIB" -headers "$HEADERS_DIR"
  -library "$MACOS_LIB" -headers "$HEADERS_DIR"
)
if [[ "$SIM_ONLY" -eq 0 ]]; then
  XCFRAMEWORK_ARGS=(
    -library "$DEVICE_LIB" -headers "$HEADERS_DIR"
    "${XCFRAMEWORK_ARGS[@]}"
  )
fi

xcodebuild -create-xcframework \
  "${XCFRAMEWORK_ARGS[@]}" \
  -output "$XCFRAMEWORK_OUT"

echo "== done =="
echo "Raw bindgen output:        $GEN_DIR/"
echo "xcframework:                $XCFRAMEWORK_OUT (ios device + ios simulator + macos-arm64 slices)"
echo "Swift bindings source:      $SWIFT_SOURCES_DIR/nmp_ffi.swift (Package.swift wiring is the next builder's job)"
