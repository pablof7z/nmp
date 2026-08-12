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
#
# CARGO_TARGET_DIR is honored when supplied by the caller.

set -euo pipefail

REPO_ROOT=$(git rev-parse --show-toplevel)
cd "$REPO_ROOT"

# NMP ships one native library, so these are constants rather than knobs.
CRATE=nmp-ffi
LIB_STEM=nmp_ffi
BINDGEN_NAME=uniffi-bindgen
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

LIB_NAME="lib$LIB_STEM.$LIB_EXT"

# Cargo resolves a relative CARGO_TARGET_DIR from its working directory. The
# script runs Cargo at the repository root, so make it absolute first.
TARGET_DIR_VALUE=${CARGO_TARGET_DIR:-target}
if [[ "$TARGET_DIR_VALUE" == /* ]]; then
  TARGET_DIR="$TARGET_DIR_VALUE"
else
  TARGET_DIR="$REPO_ROOT/$TARGET_DIR_VALUE"
fi
HOST_TARGET=$(rustc -vV | sed -n 's/^host: //p')
if [[ -z "$HOST_TARGET" ]]; then
  echo "error: rustc -vV did not report a host target" >&2
  exit 1
fi
echo "== 1. cargo build (release, host triple) =="
cargo fetch --locked
CARGO_TARGET_DIR="$TARGET_DIR" \
  cargo build --frozen -p "$CRATE" --no-default-features --all-features --release --target "$HOST_TARGET"
RELEASE_DIR="$TARGET_DIR/$HOST_TARGET/release"

HOST_LIB="$RELEASE_DIR/$LIB_NAME"
if [[ ! -f "$HOST_LIB" ]]; then
  echo "error: expected $HOST_LIB -- check nmp-ffi's [lib] crate-type includes cdylib" >&2
  exit 1
fi
BINDGEN="$RELEASE_DIR/$BINDGEN_NAME"
if [[ ! -x "$BINDGEN" ]]; then
  echo "error: expected executable $BINDGEN under $RELEASE_DIR" >&2
  exit 1
fi

echo "== 2. uniffi-bindgen (library mode) -> Kotlin bindings =="
rm -rf "$GEN_DIR"
mkdir -p "$GEN_DIR"
"$BINDGEN" generate \
  --library "$HOST_LIB" \
  --language kotlin \
  --out-dir "$GEN_DIR"

echo "== 3. copy generated bindings into the Gradle module =="
KOTLIN_SOURCES_DIR="$KOTLIN_PKG_DIR/src/main/kotlin/uniffi/$LIB_STEM"
rm -rf "$KOTLIN_SOURCES_DIR"
mkdir -p "$KOTLIN_SOURCES_DIR"
cp "$GEN_DIR/uniffi/$LIB_STEM/$LIB_STEM.kt" "$KOTLIN_SOURCES_DIR/"

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
echo "Kotlin bindings source: $KOTLIN_SOURCES_DIR/$LIB_STEM.kt"
echo "Native lib resource:    $RESOURCE_DIR/$LIB_NAME (JNA prefix: $JNA_PREFIX)"
echo "Run the smoke test with: (cd $KOTLIN_PKG_DIR && ./gradlew test)"
