#!/usr/bin/env bash
# #40: cargo -> Kotlin/JVM bindings + native lib -> (a later builder wires)
# an Android AAR, once the M5 human verdict gates that work. Mirrors
# scripts/build-swift-xcframework.sh's shape for the falsifier's own JVM
# target:
#
# 1. Build the nmp-ffi cdylib for the HOST triple (JVM target -- this is a
#    desktop-JVM smoke-test falsifier, not the M6 Android AAR; cargo-ndk
#    cross-compiling to Android ABIs is that later builder's job, see
#    Packages/NMPKotlin/README.md).
# 2. Run uniffi-bindgen in LIBRARY mode against the compiled cdylib to
#    generate the Kotlin bindings (uniffi/nmp_ffi/nmp_ffi.kt) -- no .udl
#    file, metadata is read straight out of the compiled binary, same as
#    the Swift path.
# 3. Copy the generated bindings into Packages/NMPKotlin/src/main/kotlin/
#    (a source directory Gradle's default source set already scans).
# 4. Copy the native lib into Packages/NMPKotlin/src/main/resources/
#    <jna-platform-prefix>/ -- JNA's `Native.load` resolves a bundled
#    native lib from a classpath resource at exactly that path with no
#    further wiring (no jna.library.path system property needed), which is
#    why this script computes the prefix from `uname` rather than hardcoding
#    one platform.
#
# Usage: scripts/build-kotlin-jvm.sh

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

CRATE=nmp-ffi
GEN_DIR=gen-kotlin
KOTLIN_PKG_DIR=Packages/NMPKotlin

case "$(uname -s)" in
  Darwin) LIB_EXT=dylib ;;
  Linux) LIB_EXT=so ;;
  *)
    echo "error: unsupported OS $(uname -s) -- this falsifier targets desktop JVM (macOS/Linux) only" >&2
    exit 1
    ;;
esac

LIB_NAME="libnmp_ffi.$LIB_EXT"

echo "== 1. cargo build (release, host triple) =="
cargo build -p "$CRATE" --release

HOST_LIB="target/release/$LIB_NAME"
if [[ ! -f "$HOST_LIB" ]]; then
  echo "error: expected $HOST_LIB after cargo build -- check nmp-ffi's [lib] crate-type includes cdylib" >&2
  exit 1
fi

echo "== 2. uniffi-bindgen (library mode) -> Kotlin bindings =="
rm -rf "$GEN_DIR"
mkdir -p "$GEN_DIR"
cargo run -p "$CRATE" --bin uniffi-bindgen -- generate \
  --library "$HOST_LIB" \
  --language kotlin \
  --out-dir "$GEN_DIR"

echo "== 3. copy generated bindings into the Gradle module =="
KOTLIN_SOURCES_DIR="$KOTLIN_PKG_DIR/src/main/kotlin/uniffi/nmp_ffi"
rm -rf "$KOTLIN_SOURCES_DIR"
mkdir -p "$KOTLIN_SOURCES_DIR"
cp "$GEN_DIR/uniffi/nmp_ffi/nmp_ffi.kt" "$KOTLIN_SOURCES_DIR/"

echo "== 4. copy the native lib into a JNA-resolvable resource path =="
# JNA's resource-prefix naming (Platform.RESOURCE_PREFIX): "<os>-<arch>".
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
RESOURCE_DIR="$KOTLIN_PKG_DIR/src/main/resources/$JNA_PREFIX"
rm -rf "$KOTLIN_PKG_DIR/src/main/resources"
mkdir -p "$RESOURCE_DIR"
cp "$HOST_LIB" "$RESOURCE_DIR/"
# macOS also ships a plain "darwin" resource dir for JNA versions that
# still look for the pre-multi-arch fat-binary convention; harmless
# duplication, both point at the same file.
if [[ "$JNA_OS" == "darwin" ]]; then
  mkdir -p "$KOTLIN_PKG_DIR/src/main/resources/darwin"
  cp "$HOST_LIB" "$KOTLIN_PKG_DIR/src/main/resources/darwin/"
fi

echo "== done =="
echo "Raw bindgen output:    $GEN_DIR/"
echo "Kotlin bindings source: $KOTLIN_SOURCES_DIR/nmp_ffi.kt"
echo "Native lib resource:    $RESOURCE_DIR/$LIB_NAME (JNA prefix: $JNA_PREFIX)"
echo "Run the smoke test with: (cd $KOTLIN_PKG_DIR && ./gradlew test)"
