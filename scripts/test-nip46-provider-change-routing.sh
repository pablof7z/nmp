#!/usr/bin/env bash
# Deterministic path and failure falsifiers for the NIP-46 CI change router.

set -euo pipefail

ROOT=$(git rev-parse --show-toplevel)
CHECKER="$ROOT/scripts/check-nip46-provider-changes.sh"
TEMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/nmp-nip46-change-routing.XXXXXX")
FIXTURE_ROOT="$TEMP_ROOT/repo"
trap 'rm -rf "$TEMP_ROOT"' EXIT

fail() {
  echo "NIP-46 provider change routing test: $*" >&2
  exit 1
}

git -C "$TEMP_ROOT" init -q repo
git -C "$FIXTURE_ROOT" config user.email nip46-routing@example.invalid
git -C "$FIXTURE_ROOT" config user.name "NIP-46 routing test"
mkdir -p "$FIXTURE_ROOT/crates/nmp-ffi/src"
printf 'seed\n' > "$FIXTURE_ROOT/crates/nmp-ffi/src/lib.rs"
git -C "$FIXTURE_ROOT" add .
git -C "$FIXTURE_ROOT" commit -qm base
BASE=$(git -C "$FIXTURE_ROOT" rev-parse HEAD)

classify_paths() {
  local label=$1 expected=$2
  shift 2

  git -C "$FIXTURE_ROOT" switch -q -C "case-$label" "$BASE"
  git -C "$FIXTURE_ROOT" clean -qfd
  local directory path
  for path in "$@"; do
    directory=${path%/*}
    if [[ "$directory" != "$path" ]]; then
      mkdir -p "$FIXTURE_ROOT/$directory"
    fi
    printf 'changed by %s\n' "$label" > "$FIXTURE_ROOT/$path"
  done
  git -C "$FIXTURE_ROOT" add .
  git -C "$FIXTURE_ROOT" commit -qm "$label"

  local actual
  actual=$(NMP_ROOT="$FIXTURE_ROOT" "$CHECKER" "$BASE" HEAD)
  [[ "$actual" == "$expected" ]] ||
    fail "$label: expected $expected, got $actual"
}

classify_paths unrelated false \
  README.md \
  docs/ci-notes.md \
  skills/nmp-dev/SKILL.md \
  Packages/NMP/Package.swift \
  .github/workflows/ci.yml \
  scripts/check-readme-status.sh

relevant_paths=(
  .github/workflows/nip46-provider.yml
  Cargo.toml
  Cargo.lock
  rust-toolchain.toml
  crates/nmp-engine/src/lib.rs
  Packages/NMPKotlin/nip46/build.gradle.kts
  scripts/build-component-release.sh
  scripts/build-kotlin-jvm.sh
  scripts/build-kotlin-nip46-jvm.sh
  scripts/check-nip46-artifact-inventory.sh
  scripts/check-nip46-provider-changes.sh
  scripts/test-component-identity-build.sh
  scripts/test-nip46-provider-removal.sh
  scripts/lib/require-commands.sh
)
index=0
for path in "${relevant_paths[@]}"; do
  index=$((index + 1))
  classify_paths "relevant-$index" true "$path"
done

classify_paths mixed true docs/ci-notes.md crates/nmp-ffi/src/lib.rs

git -C "$FIXTURE_ROOT" switch -q -C case-relevant-deletion "$BASE"
rm "$FIXTURE_ROOT/crates/nmp-ffi/src/lib.rs"
git -C "$FIXTURE_ROOT" commit -qam "relevant deletion"
deleted_path=$(
  NMP_ROOT="$FIXTURE_ROOT" "$CHECKER" "$BASE" HEAD
)
[[ "$deleted_path" == true ]] ||
  fail "deleting a relevant path must run the proofs"

same_commit=$(
  NMP_ROOT="$FIXTURE_ROOT" "$CHECKER" "$BASE" "$BASE"
)
[[ "$same_commit" == false ]] ||
  fail "an empty diff should skip the proofs"

invalid_ref=$(
  NMP_ROOT="$FIXTURE_ROOT" "$CHECKER" missing-base HEAD 2>/dev/null
)
[[ "$invalid_ref" == true ]] ||
  fail "an unavailable diff must run the proofs"

echo "NIP-46 provider change routing test: unrelated changes skipped, relevant changes and classifier failure ran"
