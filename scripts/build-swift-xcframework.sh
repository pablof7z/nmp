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
PAIRED_NIP46=${NMP_SWIFT_PAIRED_NIP46:-0}

if [[ "$CRATE" != nmp-ffi ]]; then
  echo "error: Swift component assembly starts from the standalone nmp-ffi root" >&2
  exit 2
fi

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
# script runs Cargo at the repository root. Treat it as a BASE only: the
# managed builder reuses a component cache but returns a fresh sealed
# artifact snapshot for each target. Packaging never reads that cache.
TARGET_DIR_VALUE=${CARGO_TARGET_DIR:-target}
if [[ "$TARGET_DIR_VALUE" == /* ]]; then
  TARGET_DIR="$TARGET_DIR_VALUE"
else
  TARGET_DIR="$REPO_ROOT/$TARGET_DIR_VALUE"
fi

build_target() {
  local target=$1 core provider=
  core=$(
    "$REPO_ROOT/scripts/build-component-release.sh" \
      "$TARGET_DIR" "$target" nmp-ffi
  )
  if [[ "$PAIRED_NIP46" == 1 ]]; then
    provider=$(
      "$REPO_ROOT/scripts/build-component-release.sh" \
        "$TARGET_DIR" "$target" \
        --core-artifact "$core/libnmp_ffi.a" nmp-nip46-ffi
    )
  fi
  printf '%s\t%s\n' "$core" "$provider"
}

build_target_without_macos() {
  env -u MACOSX_DEPLOYMENT_TARGET bash -c '
    build_target "$1"
  ' _ "$1"
}
export -f build_target
export REPO_ROOT TARGET_DIR PAIRED_NIP46

component_manifest_identity() {
  local manifest=$1 identity
  identity=$(
    python3 -c '
import json, sys
value = json.load(open(sys.argv[1], "rb"))
identity = value.get("identity")
if isinstance(identity, str):
    print(identity)
' "$manifest"
  )
  if [[ ! "$identity" =~ ^nmp-[a-z0-9-]+-component-v2-[0-9a-f]{64}$ ]]; then
    echo "error: $manifest has no exact v2 component identity" >&2
    exit 1
  fi
  printf '%s\n' "$identity"
}

echo "== 1. cargo build (isolated release) =="
cargo fetch --locked
if [[ "$MODE" != macos ]]; then
  SIM_ARM_BUILD=$(build_target_without_macos "$SIM_ARM_TARGET")
  IFS=$'\t' read -r SIM_ARM_COMPONENT_ARTIFACT_DIR \
    PAIRED_SIM_ARM_COMPONENT_ARTIFACT_DIR <<<"$SIM_ARM_BUILD"
  SIM_X86_BUILD=$(build_target_without_macos "$SIM_X86_TARGET")
  IFS=$'\t' read -r SIM_X86_COMPONENT_ARTIFACT_DIR \
    PAIRED_SIM_X86_COMPONENT_ARTIFACT_DIR <<<"$SIM_X86_BUILD"
fi
MACOS_BUILD=$(
  MACOSX_DEPLOYMENT_TARGET="$MACOS_DEPLOYMENT_TARGET" \
  CFLAGS="$MACOS_CFLAGS" \
  CXXFLAGS="$MACOS_CXXFLAGS" \
    build_target "$MACOS_TARGET"
)
IFS=$'\t' read -r MACOS_COMPONENT_ARTIFACT_DIR \
  PAIRED_MACOS_COMPONENT_ARTIFACT_DIR <<<"$MACOS_BUILD"
if [[ "$MODE" == all ]]; then
  DEVICE_BUILD=$(build_target_without_macos "$DEVICE_TARGET")
  IFS=$'\t' read -r DEVICE_COMPONENT_ARTIFACT_DIR \
    PAIRED_DEVICE_COMPONENT_ARTIFACT_DIR <<<"$DEVICE_BUILD"
fi

cleanup_component_artifacts() {
  local directory
  for directory in \
    "${SIM_ARM_COMPONENT_ARTIFACT_DIR:-}" \
    "${SIM_X86_COMPONENT_ARTIFACT_DIR:-}" \
    "${MACOS_COMPONENT_ARTIFACT_DIR:-}" \
    "${DEVICE_COMPONENT_ARTIFACT_DIR:-}" \
    "${PAIRED_SIM_ARM_COMPONENT_ARTIFACT_DIR:-}" \
    "${PAIRED_SIM_X86_COMPONENT_ARTIFACT_DIR:-}" \
    "${PAIRED_MACOS_COMPONENT_ARTIFACT_DIR:-}" \
    "${PAIRED_DEVICE_COMPONENT_ARTIFACT_DIR:-}"
  do
    if [[ -n "$directory" && -d "$directory" ]]; then
      chmod -R u+w "$directory" 2>/dev/null || true
      rm -r "$directory"
    fi
  done
  for directory in \
    "${CORE_XCFRAMEWORK_STAGE:-}" \
    "${PAIRED_XCFRAMEWORK_STAGE:-}"
  do
    if [[ -n "$directory" && -d "$directory" ]]; then
      chmod -R u+w "$directory" 2>/dev/null || true
      rm -r "$directory"
    fi
  done
}
trap cleanup_component_artifacts EXIT

SIM_ARM_LIB="${SIM_ARM_COMPONENT_ARTIFACT_DIR:-}/$LIB_NAME"
SIM_X86_LIB="${SIM_X86_COMPONENT_ARTIFACT_DIR:-}/$LIB_NAME"
MACOS_LIB="$MACOS_COMPONENT_ARTIFACT_DIR/$LIB_NAME"
DEVICE_LIB="${DEVICE_COMPONENT_ARTIFACT_DIR:-}/$LIB_NAME"

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
cp "$GEN_DIR/nmp_component_interfaceFFI.h" "$HEADERS_DIR/"
awk '1' \
  "$GEN_DIR/${LIB_STEM}FFI.modulemap" \
  "$GEN_DIR/nmp_component_interfaceFFI.modulemap" \
  >"$HEADERS_DIR/module.modulemap"

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
cp "$GEN_DIR/nmp_component_interface.swift" \
  "$SWIFT_SOURCES_DIR/nmp_component_interface.swift"

echo "== 4. xcodebuild -create-xcframework =="
mkdir -p "$(dirname "$XCFRAMEWORK_OUT")"
if [[ -d $XCFRAMEWORK_OUT ]]; then
  chmod -R u+w "$XCFRAMEWORK_OUT" 2>/dev/null || true
  rm -r "$XCFRAMEWORK_OUT"
fi
XCFRAMEWORK_BUILD_OUT="$XCFRAMEWORK_OUT"
if [[ "$MODE" == macos ]]; then
  XCFRAMEWORK_STAGE_PARENT="$TARGET_DIR/nmp-package-candidates"
  mkdir -p "$XCFRAMEWORK_STAGE_PARENT"
  CORE_XCFRAMEWORK_STAGE=$(
    mktemp -d "$XCFRAMEWORK_STAGE_PARENT/nmp-xcframework.XXXXXX"
  )
  XCFRAMEWORK_BUILD_OUT="$CORE_XCFRAMEWORK_STAGE/$(basename "$XCFRAMEWORK_OUT")"
fi

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
  -output "$XCFRAMEWORK_BUILD_OUT"

if [[ "$MODE" == macos ]]; then
  chmod -R a-w "$XCFRAMEWORK_BUILD_OUT"
  WITNESS_TOOL="$TARGET_DIR/nmp-component-artifact-witness-tool/release/nmp-component-artifact-witness"
  "$REPO_ROOT/scripts/verify-component-manifests.py" \
    --witness-tool "$WITNESS_TOOL" \
    --artifact "$XCFRAMEWORK_BUILD_OUT/macos-arm64/$LIB_NAME" \
    "$MACOS_COMPONENT_ARTIFACT_DIR/component-manifest.json" \
    --witness "$MACOS_LIB.witness.json" \
    --publish-payload \
    --publish-tree "$XCFRAMEWORK_BUILD_OUT" "$XCFRAMEWORK_OUT" >/dev/null
fi

if [[ "$PAIRED_NIP46" == 1 ]]; then
  PAIRED_CRATE=nmp-nip46-ffi
  PAIRED_LIB_STEM=nmp_nip46_ffi
  PAIRED_LIB_NAME="lib$PAIRED_LIB_STEM.a"
  PAIRED_GEN_DIR="$REPO_ROOT/gen-nip46"
  PAIRED_SWIFT_PACKAGE_DIR="$REPO_ROOT/Packages/NMPNip46"
  PAIRED_SWIFT_SOURCE_DIR="$PAIRED_SWIFT_PACKAGE_DIR/Sources/NMPNip46FFI"
  PAIRED_XCFRAMEWORK_OUT="$PAIRED_SWIFT_PACKAGE_DIR/NMPNip46.xcframework"
  PAIRED_MACOS_LIB="$PAIRED_MACOS_COMPONENT_ARTIFACT_DIR/$PAIRED_LIB_NAME"

  echo "== 5. verify paired provider macOS deployment target ($MACOS_DEPLOYMENT_TARGET) =="
  "$DEPLOYMENT_CHECKER" "$PAIRED_MACOS_LIB"

  if [[ "$MODE" != macos ]]; then
    echo "== 6. lipo the paired provider simulator arches =="
    PAIRED_FAT_SIM_DIR="$TARGET_DIR/ios-sim-fat-nip46"
    mkdir -p "$PAIRED_FAT_SIM_DIR"
    PAIRED_FAT_SIM_LIB="$PAIRED_FAT_SIM_DIR/$PAIRED_LIB_NAME"
    lipo -create \
      "$PAIRED_SIM_ARM_COMPONENT_ARTIFACT_DIR/$PAIRED_LIB_NAME" \
      "$PAIRED_SIM_X86_COMPONENT_ARTIFACT_DIR/$PAIRED_LIB_NAME" \
      -output "$PAIRED_FAT_SIM_LIB"
    lipo -info "$PAIRED_FAT_SIM_LIB"
    PAIRED_BINDGEN_LIB="$PAIRED_SIM_ARM_COMPONENT_ARTIFACT_DIR/$PAIRED_LIB_NAME"
  else
    echo "== 6. paired provider simulator lipo skipped (macOS only) =="
    PAIRED_BINDGEN_LIB="$PAIRED_MACOS_LIB"
  fi

  echo "== 7. paired provider uniffi-bindgen -> Swift bindings =="
  mkdir -p "$PAIRED_GEN_DIR"
  env -u MACOSX_DEPLOYMENT_TARGET \
    cargo run --locked -p "$PAIRED_CRATE" --bin nmp-nip46-uniffi-bindgen -- generate \
    --library "$PAIRED_BINDGEN_LIB" \
    --language swift \
    --out-dir "$PAIRED_GEN_DIR"

  PAIRED_HEADERS_DIR="$TARGET_DIR/ios-nip46-ffi-headers"
  rm -rf "$PAIRED_HEADERS_DIR"
  mkdir -p "$PAIRED_HEADERS_DIR"
  cp "$PAIRED_GEN_DIR/${PAIRED_LIB_STEM}FFI.h" "$PAIRED_HEADERS_DIR/"
  cp "$PAIRED_GEN_DIR/${PAIRED_LIB_STEM}FFI.modulemap" \
    "$PAIRED_HEADERS_DIR/module.modulemap"

  mkdir -p "$PAIRED_SWIFT_SOURCE_DIR"
  awk '
    { print }
    $0 == "import Foundation" { print "import NMPFFI" }
  ' "$PAIRED_GEN_DIR/$PAIRED_LIB_STEM.swift" \
    > "$PAIRED_SWIFT_SOURCE_DIR/$PAIRED_LIB_STEM.swift.tmp"
  mv "$PAIRED_SWIFT_SOURCE_DIR/$PAIRED_LIB_STEM.swift.tmp" \
    "$PAIRED_SWIFT_SOURCE_DIR/$PAIRED_LIB_STEM.swift"

  PAIRED_SWIFT_SOURCE="$PAIRED_SWIFT_SOURCE_DIR/$PAIRED_LIB_STEM.swift"
  PAIRED_MACOS_IDENTITY=$(
    component_manifest_identity \
      "$PAIRED_MACOS_COMPONENT_ARTIFACT_DIR/component-manifest.json"
  )
  {
    printf '\n// Exact identity of the native provider packaged with this binding.\n'
    if [[ "$MODE" == macos ]]; then
      printf 'public let nmpNip46PackagedComponentIdentity = "%s"\n' \
        "$PAIRED_MACOS_IDENTITY"
    else
      PAIRED_SIM_ARM_IDENTITY=$(
        component_manifest_identity \
          "$PAIRED_SIM_ARM_COMPONENT_ARTIFACT_DIR/component-manifest.json"
      )
      PAIRED_SIM_X86_IDENTITY=$(
        component_manifest_identity \
          "$PAIRED_SIM_X86_COMPONENT_ARTIFACT_DIR/component-manifest.json"
      )
      printf '#if os(macOS) && arch(arm64)\n'
      printf 'public let nmpNip46PackagedComponentIdentity = "%s"\n' \
        "$PAIRED_MACOS_IDENTITY"
      printf '#elseif os(iOS) && targetEnvironment(simulator) && arch(arm64)\n'
      printf 'public let nmpNip46PackagedComponentIdentity = "%s"\n' \
        "$PAIRED_SIM_ARM_IDENTITY"
      printf '#elseif os(iOS) && targetEnvironment(simulator) && arch(x86_64)\n'
      printf 'public let nmpNip46PackagedComponentIdentity = "%s"\n' \
        "$PAIRED_SIM_X86_IDENTITY"
      if [[ "$MODE" == all ]]; then
        PAIRED_DEVICE_IDENTITY=$(
          component_manifest_identity \
            "$PAIRED_DEVICE_COMPONENT_ARTIFACT_DIR/component-manifest.json"
        )
        printf '#elseif os(iOS) && !targetEnvironment(simulator) && arch(arm64)\n'
        printf 'public let nmpNip46PackagedComponentIdentity = "%s"\n' \
          "$PAIRED_DEVICE_IDENTITY"
      fi
      printf '#else\n'
      printf '#error("NMPNip46 has no packaged native component for this target")\n'
      printf '#endif\n'
    fi
  } >>"$PAIRED_SWIFT_SOURCE"

  echo "== 8. paired provider xcodebuild -create-xcframework =="
  if [[ -d $PAIRED_XCFRAMEWORK_OUT ]]; then
    chmod -R u+w "$PAIRED_XCFRAMEWORK_OUT" 2>/dev/null || true
    rm -r "$PAIRED_XCFRAMEWORK_OUT"
  fi
  PAIRED_XCFRAMEWORK_BUILD_OUT="$PAIRED_XCFRAMEWORK_OUT"
  if [[ "$MODE" == macos ]]; then
    PAIRED_XCFRAMEWORK_STAGE_PARENT="$TARGET_DIR/nmp-package-candidates"
    mkdir -p "$PAIRED_XCFRAMEWORK_STAGE_PARENT"
    PAIRED_XCFRAMEWORK_STAGE=$(
      mktemp -d "$PAIRED_XCFRAMEWORK_STAGE_PARENT/nmp-nip46-xcframework.XXXXXX"
    )
    PAIRED_XCFRAMEWORK_BUILD_OUT="$PAIRED_XCFRAMEWORK_STAGE/$(basename "$PAIRED_XCFRAMEWORK_OUT")"
  fi
  PAIRED_XCFRAMEWORK_ARGS=(-library "$PAIRED_MACOS_LIB" -headers "$PAIRED_HEADERS_DIR")
  PAIRED_SLICES=macos-arm64
  if [[ "$MODE" != macos ]]; then
    PAIRED_XCFRAMEWORK_ARGS=(
      -library "$PAIRED_FAT_SIM_LIB" -headers "$PAIRED_HEADERS_DIR"
      "${PAIRED_XCFRAMEWORK_ARGS[@]}"
    )
    PAIRED_SLICES="ios-simulator + $PAIRED_SLICES"
  fi
  if [[ "$MODE" == all ]]; then
    PAIRED_XCFRAMEWORK_ARGS=(
      -library "$PAIRED_DEVICE_COMPONENT_ARTIFACT_DIR/$PAIRED_LIB_NAME"
      -headers "$PAIRED_HEADERS_DIR"
      "${PAIRED_XCFRAMEWORK_ARGS[@]}"
    )
    PAIRED_SLICES="ios-device + $PAIRED_SLICES"
  fi

  xcodebuild -create-xcframework \
    "${PAIRED_XCFRAMEWORK_ARGS[@]}" \
    -output "$PAIRED_XCFRAMEWORK_BUILD_OUT"
  if [[ "$MODE" == macos ]]; then
    chmod -R a-w "$PAIRED_XCFRAMEWORK_BUILD_OUT"
    "$REPO_ROOT/scripts/verify-component-manifests.py" \
      --witness-tool "$WITNESS_TOOL" \
      --artifact "$XCFRAMEWORK_OUT/macos-arm64/$LIB_NAME" \
      "$MACOS_COMPONENT_ARTIFACT_DIR/component-manifest.json" \
      --witness "$MACOS_LIB.witness.json" \
      --artifact \
      "$PAIRED_XCFRAMEWORK_BUILD_OUT/macos-arm64/$PAIRED_LIB_NAME" \
      "$PAIRED_MACOS_COMPONENT_ARTIFACT_DIR/component-manifest.json" \
      --witness "$PAIRED_MACOS_LIB.witness.json" \
      --forbid-symbols \
      "$PAIRED_MACOS_COMPONENT_ARTIFACT_DIR/component-interface-forbidden-symbols.nul" \
      --localization-source "$XCFRAMEWORK_OUT/macos-arm64/$LIB_NAME" \
      --localization-plan \
      "$PAIRED_MACOS_COMPONENT_ARTIFACT_DIR/component-interface-localization-plan.json" \
      --publish-payload \
      --publish-tree "$PAIRED_XCFRAMEWORK_BUILD_OUT" \
      "$PAIRED_XCFRAMEWORK_OUT" >/dev/null
  fi
  echo "paired provider xcframework: $PAIRED_XCFRAMEWORK_OUT ($PAIRED_SLICES)"
fi

echo "== done =="
echo "Cargo target directory:    $TARGET_DIR"
echo "Raw bindgen output:        $GEN_DIR/"
echo "xcframework:               $XCFRAMEWORK_OUT ($SLICES)"
echo "Swift bindings source:     $SWIFT_SOURCE"
