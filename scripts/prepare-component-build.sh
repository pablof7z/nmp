#!/usr/bin/env bash
# Create the isolated release target and Cargo-observed marker consumed by
# nmp-ffi/build.rs. This is the only supported door for native component
# release artifacts.

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
    ;;
  "nmp-ffi nmp-nip46-ffi")
    PACKAGE_SET=nip46
    ;;
  *)
    echo "component-build: unsupported package roots: $PACKAGE_NAMES" >&2
    exit 1
    ;;
esac

COMPONENT_TARGET_DIR="$BASE_TARGET_DIR/nmp-component-build/$PACKAGE_SET"
MARKER_DIR="$COMPONENT_TARGET_DIR/.nmp-component-build-v1"
MARKER="$MARKER_DIR/$TARGET"
mkdir -p "$MARKER_DIR"

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
mv "$AUTH_TEMP" "$MARKER_DIR/.authorization"

printf '%s\n' "$COMPONENT_TARGET_DIR"
printf '%s\n' "$AUTH_TOKEN"
