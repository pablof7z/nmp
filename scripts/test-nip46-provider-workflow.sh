#!/usr/bin/env bash
# Mutation falsifiers for the NIP-46 provider workflow ownership contract.

set -euo pipefail

ROOT=$(git rev-parse --show-toplevel)
CHECKER="$ROOT/scripts/check-nip46-provider-boundary.sh"
TEMP_ROOT=$(mktemp -d)
FIXTURE_ROOT="$TEMP_ROOT/repo"
trap 'rm -r "$TEMP_ROOT"' EXIT

fail() {
  echo "NIP-46 provider workflow test: $*" >&2
  exit 1
}

reset_fixture() {
  rm -rf "$FIXTURE_ROOT"
  mkdir -p "$FIXTURE_ROOT/.github/workflows"
  cp "$ROOT/.github/workflows/macos-qualification.yml" \
    "$FIXTURE_ROOT/.github/workflows/"
  cp "$ROOT/.github/workflows/nip46-provider.yml" \
    "$FIXTURE_ROOT/.github/workflows/"
}

expect_failure() {
  local label=$1
  local expected=$2
  local output
  if output=$(bash "$CHECKER" --workflows-only "$FIXTURE_ROOT" 2>&1); then
    fail "$label mutation unexpectedly passed"
  fi
  grep -Fq -- "$expected" <<< "$output" ||
    fail "$label mutation failed for the wrong reason: $output"
}

reset_fixture
bash "$CHECKER" --workflows-only "$FIXTURE_ROOT"

sed -i.bak \
  's#matched_provider=Packages/NMPNip46/NMPNip46\.xcframework/#matched_provider=Packages/NMPNip46/removed.xcframework/#g' \
  "$FIXTURE_ROOT/.github/workflows/macos-qualification.yml"
rm "$FIXTURE_ROOT/.github/workflows/macos-qualification.yml.bak"
expect_failure "removed Swift packaged provider output" \
  "Swift provider workflow does not audit the packaged provider XCFramework"

reset_fixture
sed -i.bak \
  's#scripts/check-nip46-component-identity\.sh#scripts/removed-component-identity.sh#g' \
  "$FIXTURE_ROOT/.github/workflows/macos-qualification.yml"
rm "$FIXTURE_ROOT/.github/workflows/macos-qualification.yml.bak"
expect_failure "removed Swift identity proof" \
  "Swift provider workflow does not prove matched component identity"

reset_fixture
sed -i.bak \
  's#scripts/check-nip46-artifact-inventory\.sh#scripts/removed-artifact-inventory.sh#g' \
  "$FIXTURE_ROOT/.github/workflows/macos-qualification.yml"
rm "$FIXTURE_ROOT/.github/workflows/macos-qualification.yml.bak"
expect_failure "removed Swift inventory proof" \
  "Swift provider workflow does not audit packaged component inventory"

reset_fixture
sed -i.bak \
  's#scripts/check-nip46-artifact-inventory\.sh#scripts/removed-artifact-inventory.sh#g' \
  "$FIXTURE_ROOT/.github/workflows/nip46-provider.yml"
rm "$FIXTURE_ROOT/.github/workflows/nip46-provider.yml.bak"
expect_failure "removed Kotlin inventory proof" \
  "Kotlin provider workflow does not audit packaged component inventory"

reset_fixture
sed -i.bak \
  's#matched_provider=Packages/NMPNip46/NMPNip46\.xcframework/\$slice_directory/libnmp_nip46_ffi\.a#matched_provider=target/nmp-component-build/nip46/release/libnmp_nip46_ffi.a#' \
  "$FIXTURE_ROOT/.github/workflows/macos-qualification.yml"
rm "$FIXTURE_ROOT/.github/workflows/macos-qualification.yml.bak"
expect_failure "mutable Cargo-cache Swift audit" \
  "provider workflow still audits mutable Cargo-cache libraries"

echo "NIP-46 provider workflow test: baseline and five mutations passed"
