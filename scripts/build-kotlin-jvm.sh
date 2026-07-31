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

CRATE=${NMP_FFI_CRATE:-nmp-ffi}
LIB_STEM=${NMP_FFI_LIB_STEM:-nmp_ffi}
BINDGEN_NAME=${NMP_UNIFFI_BINDGEN_BIN:-uniffi-bindgen}
GEN_DIR=${NMP_KOTLIN_GEN_DIR:-gen-kotlin}
KOTLIN_PKG_DIR=${NMP_KOTLIN_MODULE_DIR:-Packages/NMPKotlin}

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
# script runs Cargo at the repository root. Treat it as a BASE only: the
# managed builder uses a component-specific cache and returns a fresh sealed
# artifact snapshot. Packaging never reads the reusable Cargo target itself.
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
echo "== 1. cargo build (isolated release, host triple) =="
cargo fetch --locked
OWNED_COMPONENT_ARTIFACT_DIRS=("")
if [[ -n ${NMP_COMPONENT_ARTIFACT_DIR:-} ]]; then
  COMPONENT_ARTIFACT_DIR=$NMP_COMPONENT_ARTIFACT_DIR
  [[ -d "$COMPONENT_ARTIFACT_DIR" ]] || {
    echo "error: prebuilt component snapshot is missing: $COMPONENT_ARTIFACT_DIR" >&2
    exit 1
  }
elif [[ "$CRATE" == nmp-ffi ]]; then
  COMPONENT_ARTIFACT_DIR=$(
    "$REPO_ROOT/scripts/build-component-release.sh" \
      "$TARGET_DIR" "$HOST_TARGET" "$CRATE"
  )
  OWNED_COMPONENT_ARTIFACT_DIRS+=("$COMPONENT_ARTIFACT_DIR")
else
  if [[ -n ${NMP_CORE_COMPONENT_ARTIFACT:-} ]]; then
    CORE_COMPONENT_ARTIFACT=$NMP_CORE_COMPONENT_ARTIFACT
  else
    CORE_COMPONENT_ARTIFACT_DIR=$(
      "$REPO_ROOT/scripts/build-component-release.sh" \
        "$TARGET_DIR" "$HOST_TARGET" nmp-ffi
    )
    OWNED_COMPONENT_ARTIFACT_DIRS+=("$CORE_COMPONENT_ARTIFACT_DIR")
    CORE_COMPONENT_ARTIFACT="$CORE_COMPONENT_ARTIFACT_DIR/libnmp_ffi.$LIB_EXT"
  fi
  [[ -f "$CORE_COMPONENT_ARTIFACT" ]] || {
    echo "error: exact core component artifact is missing: $CORE_COMPONENT_ARTIFACT" >&2
    exit 1
  }
  COMPONENT_ARTIFACT_DIR=$(
    "$REPO_ROOT/scripts/build-component-release.sh" \
      "$TARGET_DIR" "$HOST_TARGET" \
      --core-artifact "$CORE_COMPONENT_ARTIFACT" \
      "$CRATE"
  )
  OWNED_COMPONENT_ARTIFACT_DIRS+=("$COMPONENT_ARTIFACT_DIR")
fi
cleanup_component_artifacts() {
  local directory
  for directory in "${OWNED_COMPONENT_ARTIFACT_DIRS[@]}"; do
    if [[ -n "$directory" && -d "$directory" ]]; then
      chmod -R u+w "$directory" 2>/dev/null || true
      rm -r "$directory"
    fi
  done
  if [[ -n ${RESOURCE_STAGE:-} && -d $RESOURCE_STAGE ]]; then
    chmod -R u+w "$RESOURCE_STAGE" 2>/dev/null || true
    rm -r "$RESOURCE_STAGE"
  fi
}
trap cleanup_component_artifacts EXIT

HOST_LIB="$COMPONENT_ARTIFACT_DIR/$LIB_NAME"
if [[ ! -f "$HOST_LIB" ]]; then
  echo "error: expected $HOST_LIB in the sealed component snapshot -- check nmp-ffi's [lib] crate-type includes cdylib" >&2
  exit 1
fi
BINDGEN="$COMPONENT_ARTIFACT_DIR/$BINDGEN_NAME"
if [[ ! -x "$BINDGEN" ]]; then
  echo "error: expected executable $BINDGEN in the sealed component snapshot" >&2
  exit 1
fi

echo "== 2. uniffi-bindgen (library mode) -> Kotlin bindings =="
rm -rf "$GEN_DIR"
mkdir -p "$GEN_DIR"
if [[ "$CRATE" == nmp-ffi ]]; then
  "$BINDGEN" generate \
    --library "$HOST_LIB" \
    --language kotlin \
    --out-dir "$GEN_DIR"
else
  "$BINDGEN" generate-merged-kotlin \
    --core-library "$CORE_COMPONENT_ARTIFACT" \
    --provider-library "$HOST_LIB" \
    --out-dir "$GEN_DIR"
fi

echo "== 3. copy generated bindings into the Gradle module =="
KOTLIN_SOURCES_DIR="$KOTLIN_PKG_DIR/src/main/kotlin/uniffi/$LIB_STEM"
rm -rf "$KOTLIN_SOURCES_DIR"
mkdir -p "$KOTLIN_SOURCES_DIR"
cp "$GEN_DIR/uniffi/$LIB_STEM/$LIB_STEM.kt" "$KOTLIN_SOURCES_DIR/"
if [[ "$CRATE" == nmp-ffi ]]; then
  INTERFACE_SOURCES_DIR="$KOTLIN_PKG_DIR/src/main/kotlin/uniffi/nmp_component_interface"
  rm -rf "$INTERFACE_SOURCES_DIR"
  mkdir -p "$INTERFACE_SOURCES_DIR"
  cp "$GEN_DIR/uniffi/nmp_component_interface/nmp_component_interface.kt" \
    "$INTERFACE_SOURCES_DIR/"
else
  PACKAGED_COMPONENT_IDENTITY=$(
    python3 -c '
import json, sys
value = json.load(open(sys.argv[1], "rb"))
identity = value.get("identity")
if isinstance(identity, str):
    print(identity)
' "$COMPONENT_ARTIFACT_DIR/component-manifest.json"
  )
  if [[ ! "$PACKAGED_COMPONENT_IDENTITY" =~ ^nmp-[a-z0-9-]+-component-v2-[0-9a-f]{64}$ ]]; then
    echo "error: provider manifest has no exact v2 component identity" >&2
    exit 1
  fi
  {
    printf '\n// Exact identity of the native provider packaged with this binding.\n'
    printf 'public const val NMP_NIP46_PACKAGED_COMPONENT_IDENTITY = "%s"\n' \
      "$PACKAGED_COMPONENT_IDENTITY"
  } >>"$KOTLIN_SOURCES_DIR/$LIB_STEM.kt"
fi

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
RESOURCE_ROOT="$KOTLIN_PKG_DIR/src/main/resources"
RESOURCE_STAGE_PARENT="$TARGET_DIR/nmp-package-candidates"
mkdir -p "$RESOURCE_STAGE_PARENT"
RESOURCE_STAGE=$(mktemp -d "$RESOURCE_STAGE_PARENT/$LIB_STEM-resources.XXXXXX")
mkdir -p "$RESOURCE_STAGE/$JNA_PREFIX"
cp "$HOST_LIB" "$RESOURCE_STAGE/$JNA_PREFIX/"
# macOS also ships a plain "darwin" resource dir for JNA versions that
# still look for the pre-multi-arch fat-binary convention; harmless
# duplication, both point at the same file.
if [[ "$JNA_OS" == "darwin" ]]; then
  mkdir -p "$RESOURCE_STAGE/darwin"
  cp "$HOST_LIB" "$RESOURCE_STAGE/darwin/"
fi
chmod -R a-w "$RESOURCE_STAGE"

WITNESS_TOOL="$TARGET_DIR/nmp-component-artifact-witness-tool/release/nmp-component-artifact-witness"
[[ -x $WITNESS_TOOL ]] || {
  echo "error: pinned component artifact witness tool is missing" >&2
  exit 1
}
VERIFY_ARGUMENTS=(--witness-tool "$WITNESS_TOOL")
if [[ "$CRATE" != nmp-ffi ]]; then
  CORE_COMPONENT_ARTIFACT_DIR=$(dirname "$CORE_COMPONENT_ARTIFACT")
  CORE_COMPONENT_MANIFEST="$CORE_COMPONENT_ARTIFACT_DIR/component-manifest.json"
  PUBLISHED_CORE_COMPONENT_ARTIFACT=${NMP_PUBLISHED_CORE_COMPONENT_ARTIFACT:-$CORE_COMPONENT_ARTIFACT}
  VERIFY_ARGUMENTS+=(
    --artifact "$PUBLISHED_CORE_COMPONENT_ARTIFACT" "$CORE_COMPONENT_MANIFEST"
    --witness "$CORE_COMPONENT_ARTIFACT.witness.json"
  )
  LOCALIZATION_SOURCE="$CORE_COMPONENT_ARTIFACT_DIR/libnmp_ffi.a"
  if [[ "$LOCALIZATION_SOURCE" != "$PUBLISHED_CORE_COMPONENT_ARTIFACT" ]]; then
    VERIFY_ARGUMENTS+=(
      --artifact "$LOCALIZATION_SOURCE" "$CORE_COMPONENT_MANIFEST"
    )
  fi
fi
for candidate in \
  "$RESOURCE_STAGE/$JNA_PREFIX/$LIB_NAME" \
  "$RESOURCE_STAGE/darwin/$LIB_NAME"
do
  [[ -f $candidate ]] || continue
  VERIFY_ARGUMENTS+=(
    --artifact "$candidate" "$COMPONENT_ARTIFACT_DIR/component-manifest.json"
    --witness "$HOST_LIB.witness.json"
  )
  if [[ "$CRATE" != nmp-ffi ]]; then
    VERIFY_ARGUMENTS+=(
      --forbid-symbols \
      "$COMPONENT_ARTIFACT_DIR/component-interface-forbidden-symbols.nul"
      --localization-source "$LOCALIZATION_SOURCE"
      --localization-plan \
      "$COMPONENT_ARTIFACT_DIR/component-interface-localization-plan.json"
    )
  fi
  VERIFY_ARGUMENTS+=(--publish-payload)
done
if [[ -d $RESOURCE_ROOT ]]; then
  chmod -R u+w "$RESOURCE_ROOT" 2>/dev/null || true
  rm -r "$RESOURCE_ROOT"
fi
"$REPO_ROOT/scripts/verify-component-manifests.py" \
  "${VERIFY_ARGUMENTS[@]}" \
  --publish-tree "$RESOURCE_STAGE" "$RESOURCE_ROOT" >/dev/null

echo "== done =="
echo "Raw bindgen output:    $GEN_DIR/"
echo "Kotlin bindings source: $KOTLIN_SOURCES_DIR/$LIB_STEM.kt"
echo "Native lib resource:    $RESOURCE_DIR/$LIB_NAME (JNA prefix: $JNA_PREFIX)"
echo "Run the smoke test with: (cd $KOTLIN_PKG_DIR && ./gradlew test)"
