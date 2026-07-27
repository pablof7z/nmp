#!/usr/bin/env bash
# Build one fixed native-component package set and seal the artifacts into a
# fresh snapshot before releasing the build lock. Callers package only the
# snapshot; the reusable Cargo target is never itself a package input.

set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: $0 BASE_TARGET_DIR PACKAGE_NAMES TARGET" >&2
  exit 2
fi

BASE_TARGET_DIR=$1
PACKAGE_NAMES=$2
TARGET=$3

case "$PACKAGE_NAMES" in
  nmp-ffi)
    PACKAGE_SET=core
    REQUIRED_LIB_STEMS=(nmp_ffi)
    ;;
  "nmp-ffi nmp-nip46-ffi")
    PACKAGE_SET=nip46
    REQUIRED_LIB_STEMS=(nmp_ffi nmp_nip46_ffi)
    ;;
  *)
    echo "component-build: unsupported package roots: $PACKAGE_NAMES" >&2
    exit 1
    ;;
esac

read -r -a PACKAGE_ARRAY <<< "$PACKAGE_NAMES"
PACKAGE_ARGS=()
for package_name in "${PACKAGE_ARRAY[@]}"; do
  PACKAGE_ARGS+=(-p "$package_name")
done

COMPONENT_TARGET_DIR="$BASE_TARGET_DIR/nmp-component-build/$PACKAGE_SET"
MARKER_DIR="$COMPONENT_TARGET_DIR/.nmp-component-build-v1"
MARKER="$MARKER_DIR/$TARGET"
AUTHORIZATION="$MARKER_DIR/.authorization"
LOCK_DIR="$COMPONENT_TARGET_DIR/.builder-lock"
ARTIFACT_PARENT="$BASE_TARGET_DIR/nmp-component-artifacts/$PACKAGE_SET"
ARTIFACT_SNAPSHOT=

mkdir -p "$MARKER_DIR" "$ARTIFACT_PARENT"
if ! mkdir "$LOCK_DIR" 2>/dev/null; then
  echo "component-build: another supported $PACKAGE_SET build is already using $COMPONENT_TARGET_DIR" >&2
  exit 1
fi

cleanup() {
  local exit_code=$?
  rm -f "$AUTHORIZATION"
  rmdir "$LOCK_DIR" 2>/dev/null || true
  if [[ $exit_code -ne 0 && -n "$ARTIFACT_SNAPSHOT" && -d "$ARTIFACT_SNAPSHOT" ]]; then
    chmod -R u+w "$ARTIFACT_SNAPSHOT" 2>/dev/null || true
    rm -r "$ARTIFACT_SNAPSHOT"
  fi
  exit "$exit_code"
}
trap cleanup EXIT

TEMP_MARKER="$MARKER.tmp.$$"
printf '%s\n' \
  "nmp-component-build-v1" \
  "package-set=$PACKAGE_SET" \
  "target=$TARGET" \
  "profile=release" > "$TEMP_MARKER"
mv "$TEMP_MARKER" "$MARKER"

AUTH_TEMP=$(mktemp "$MARKER_DIR/.authorization.XXXXXX")
AUTH_TOKEN=$(basename "$AUTH_TEMP")
printf '%s\n' "$AUTH_TOKEN" > "$AUTH_TEMP"
mv "$AUTH_TEMP" "$AUTHORIZATION"

# stdout is reserved for the sealed artifact directory returned to the caller.
# Cargo's human progress remains visible on stderr.
CARGO_TARGET_DIR="$COMPONENT_TARGET_DIR" \
NMP_FFI_COMPONENT_ROOT="$COMPONENT_TARGET_DIR" \
NMP_FFI_COMPONENT_AUTH="$AUTH_TOKEN" \
  cargo build --frozen "${PACKAGE_ARGS[@]}" --release --target "$TARGET" 1>&2

RELEASE_DIR="$COMPONENT_TARGET_DIR/$TARGET/release"
for stem in "${REQUIRED_LIB_STEMS[@]}"; do
  found=0
  for extension in a so dylib; do
    if [[ -f "$RELEASE_DIR/lib$stem.$extension" ]]; then
      found=1
      break
    fi
  done
  if [[ $found -eq 0 ]]; then
    echo "component-build: expected a release library for $stem under $RELEASE_DIR" >&2
    exit 1
  fi
done

ARTIFACT_SNAPSHOT=$(mktemp -d "$ARTIFACT_PARENT/$TARGET.XXXXXX")
for artifact in \
  libnmp_ffi.a libnmp_ffi.so libnmp_ffi.dylib \
  libnmp_nip46_ffi.a libnmp_nip46_ffi.so libnmp_nip46_ffi.dylib \
  uniffi-bindgen uniffi-bindgen.exe \
  nmp-nip46-uniffi-bindgen nmp-nip46-uniffi-bindgen.exe
do
  if [[ -f "$RELEASE_DIR/$artifact" ]]; then
    cp -p "$RELEASE_DIR/$artifact" "$ARTIFACT_SNAPSHOT/"
  fi
done
chmod -R a-w "$ARTIFACT_SNAPSHOT"

# Revoke the only accidental-use authorization before exposing any path to
# the caller. Later Cargo commands against the reusable target must rerun the
# build script and refuse; packaging reads only this immutable-by-convention
# snapshot.
rm -f "$AUTHORIZATION"
printf '%s\n' "$ARTIFACT_SNAPSHOT"
