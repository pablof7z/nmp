#!/usr/bin/env bash
# #877 package-removal falsifier: physically remove both NIP-46 components
# from an isolated copy, then compile the core signer/facade and the unrelated
# external-signer fixture. The working checkout is never modified.

set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
TEMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/nmp-nip46-removal.XXXXXX")
trap 'rm -rf "$TEMP_ROOT"' EXIT

COPY="$TEMP_ROOT/repo"
mkdir -p "$COPY"
(
  cd "$ROOT"
  tar \
    --exclude=.git \
    --exclude=target \
    --exclude=.build \
    --exclude=.gradle \
    --exclude='*.xcframework' \
    --exclude=gen \
    --exclude=gen-nip46 \
    --exclude=gen-kotlin \
    --exclude=gen-kotlin-nip46 \
    -cf - .
) | (cd "$COPY" && tar -xf -)

rm -rf \
  "$COPY/crates/nmp-nip46" \
  "$COPY/crates/nmp-nip46-ffi" \
  "$COPY/Packages/NMPNip46" \
  "$COPY/Packages/NMPKotlin/nip46"

awk '
  /"crates\/nmp-nip46",/ { next }
  /"crates\/nmp-nip46-ffi",/ { next }
  /^nmp-nip46[[:space:]]*=/ { next }
  /^nmp-nip46-ffi[[:space:]]*=/ { next }
  { print }
' "$COPY/Cargo.toml" > "$COPY/Cargo.toml.next"
mv "$COPY/Cargo.toml.next" "$COPY/Cargo.toml"

(
  cd "$COPY"
  cargo test -p nmp-signer -p nmp-local-signer -p nmp-ffi
  cargo test -p nmp --test signer_surface
)

echo "nip46-provider-removal: ok"
