#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
CHECK="$SCRIPT_DIR/check-surface-governance.sh"
PARITY="$SCRIPT_DIR/check-sdk-parity.sh"
MIGRATION_TEST="$SCRIPT_DIR/test-surface-migration-authorization.py"
[[ $# -eq 1 ]] || {
  echo "usage: $0 <workspace-root>" >&2
  exit 2
}
ROOT=$1
git -C "$ROOT" rev-parse --show-toplevel >/dev/null 2>&1 || {
  echo "test-surface-governance: workspace root is not a Git worktree: $ROOT" >&2
  exit 2
}
PYTHONDONTWRITEBYTECODE=1 python3 "$MIGRATION_TEST"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

fail() {
  echo "test-surface-governance: $*" >&2
  exit 1
}

workflow_permissions() {
  awk '
    /^permissions:[[:space:]]*$/ {
      inside = 1
      next
    }
    inside && /^[^[:space:]]/ {
      inside = 0
    }
    inside &&
      /^  [A-Za-z0-9_-]+:[[:space:]]*(read|write|none)[[:space:]]*$/ {
        line = $0
        sub(/^  /, "", line)
        gsub(/[[:space:]]/, "", line)
        print line
      }
  ' "$1" | LC_ALL=C sort
}

falsify_missing_base_governance_artifact() {
  local fixture="$TMP/base-trust"
  local repo="$fixture/repo"
  local trusted="$fixture/trusted-checker"
  local witness="$fixture/proposed-code-executed"
  mkdir -p "$repo" "$fixture"
  git -C "$repo" init -q
  git -C "$repo" config user.email surface@example.invalid
  git -C "$repo" config user.name SurfaceTest
  printf 'base without governance artifact\n' > "$repo/ordinary.txt"
  git -C "$repo" add ordinary.txt
  git -C "$repo" commit -qm base-without-governance-artifact
  local base
  base=$(git -C "$repo" rev-parse HEAD)

  mkdir -p "$repo/scripts"
  cat > "$repo/scripts/check-surface-governance.sh" <<EOF
#!/usr/bin/env bash
touch "$witness"
EOF
  git -C "$repo" add scripts/check-surface-governance.sh
  git -C "$repo" commit -qm proposed-head-governance

  if git -C "$repo" show \
      "$base:scripts/check-surface-governance.sh" > "$trusted" 2>/dev/null; then
    fail "missing base governance artifact did not fail closed"
  fi
  [[ ! -s "$trusted" ]] ||
    fail "missing base governance artifact was filled from the proposed head"
  [[ ! -e "$witness" ]] ||
    fail "proposed governance code executed during base extraction"
}

CATALOG_BIN=${SURFACE_CATALOG_BIN:-}
if [[ -z "$CATALOG_BIN" ]]; then
  # shellcheck source=tools/surface-toolchain.env
  source "${SURFACE_TOOLCHAIN_ENV:-$ROOT/tools/surface-toolchain.env}"
  target=${SURFACE_CATALOG_TARGET_DIR:-$TMP/catalog-target}
  cargo "+$SURFACE_RUST_TOOLCHAIN" build --quiet --locked \
    --manifest-path "${SURFACE_CATALOG_TOOL_DIR:-$ROOT/tools/surface-component-catalog}/Cargo.toml" \
    --target-dir "$target"
  CATALOG_BIN="$target/debug/nmp-surface-component-catalog"
fi
[[ -x "$CATALOG_BIN" ]]

commit_case() {
  git -C "$1" add -A
  git -C "$1" commit -qm "$2"
}

descriptor() {
  local repo=$1 key=$2 owner=$3
  mkdir -p "$repo/docs/surface/components/$key" "$repo/crates/$key-ffi/src"
  printf 'pub fn %s_fixture() {}\n' "${key//-/_}" > "$repo/crates/$key-ffi/src/lib.rs"
  if [[ $key == "$owner" ]]; then
    printf '[package]\nname = "%s-ffi"\nversion = "0.0.0"\n' "$key" \
      > "$repo/crates/$key-ffi/Cargo.toml"
  fi
  cat > "$repo/docs/surface/components/$key/component.toml" <<EOF
schema = 1
key = "$key"
state = "active"
uniffi_namespace = "${key//-/_}_ffi"
artifact_owner = "$owner"
EOF
  if [[ $key == "$owner" ]]; then
    cat >> "$repo/docs/surface/components/$key/component.toml" <<EOF
ffi_package = "$key-ffi"
ffi_manifest = "crates/$key-ffi/Cargo.toml"
library_stem = "${key//-/_}_ffi"
EOF
  fi
  cat >> "$repo/docs/surface/components/$key/component.toml" <<EOF
ffi_sources = ["crates/$key-ffi/src"]
swift_manifests = []
swift_sources = []
swift_omission_reason = "This fixture has no Swift ergonomic API."
kotlin_manifests = []
kotlin_sources = []
kotlin_omission_reason = "This fixture has no Kotlin ergonomic API."
EOF
  printf 'component "%s"\nnamespace "%s_ffi"\n' "$key" "${key//-/_}" \
    > "$repo/docs/surface/components/$key/uniffi.txt"
}

new_repo() {
  local repo=$1
  mkdir -p \
    "$repo/.github/workflows" \
    "$repo/docs/surface/components" \
    "$repo/scripts" \
    "$repo/tools/component-interface-snapshot/src" \
    "$repo/tools/rust-facade-snapshot/src" \
    "$repo/tools/surface-component-catalog/src" \
    "$repo/actual/components/alpha"
  printf '# Fixture component catalog\n' > "$repo/docs/surface/components/README.md"
  descriptor "$repo" alpha alpha
  mkdir -p "$repo/Packages/Alpha/Sources/Alpha" "$repo/Packages/AlphaKotlin/src/main/kotlin"
  printf '// package\n' > "$repo/Packages/Alpha/Package.swift"
  printf '// swift AlphaWidget\n' > "$repo/Packages/Alpha/Sources/Alpha/Alpha.swift"
  printf '// kotlin AlphaWidget\n' > "$repo/Packages/AlphaKotlin/build.gradle.kts"
  printf 'class AlphaWidget\n' > "$repo/Packages/AlphaKotlin/src/main/kotlin/Alpha.kt"
  cat > "$repo/docs/surface/components/alpha/component.toml" <<'EOF'
schema = 1
key = "alpha"
state = "active"
uniffi_namespace = "alpha_ffi"
artifact_owner = "alpha"
ffi_package = "alpha-ffi"
ffi_manifest = "crates/alpha-ffi/Cargo.toml"
library_stem = "alpha_ffi"
ffi_sources = ["crates/alpha-ffi/src"]
swift_manifests = ["Packages/Alpha/Package.swift"]
swift_sources = ["Packages/Alpha/Sources/Alpha"]
kotlin_manifests = ["Packages/AlphaKotlin/build.gradle.kts"]
kotlin_sources = ["Packages/AlphaKotlin/src/main/kotlin"]
EOF
  cat > "$repo/crates/alpha-ffi/src/lib.rs" <<'EOF'
#[derive(uniffi::Record)]
pub struct FfiAlphaWidget {
    pub value: String,
}
EOF
  printf 'schema = 1\n' > "$repo/scripts/check-sdk-parity-allowlist.toml"
  printf 'facade-v1\n' > "$repo/docs/surface/nmp-facade.txt"
  cp "$repo/docs/surface/nmp-facade.txt" "$repo/actual/nmp-facade.txt"
  cp "$repo/docs/surface/components/alpha/uniffi.txt" \
    "$repo/actual/components/alpha/uniffi.txt"
  cat > "$repo/docs/surface-change-log.md" <<'EOF'
# Surface change log

## Historical fixture

seed
EOF
  cat > "$repo/scripts/regen.sh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
[[ $1 == --output-dir && $# == 2 ]]
root=$(git rev-parse --show-toplevel)
mkdir -p "$2"
cp -R "$root/actual/." "$2"
EOF
  for path in check-sdk-parity.sh check-surface-governance.sh \
    install-surface-tools.sh regenerate-surface-snapshots.sh \
    run-surface-regeneration-governance.sh \
    test-install-surface-tools.sh test-surface-governance.sh; do
    printf '#!/usr/bin/env bash\nexit 0\n' > "$repo/scripts/$path"
  done
  printf 'name: fixture\n' > "$repo/.github/workflows/ci.yml"
  printf 'name: fixture\n' > "$repo/.github/workflows/surface-governance.yml"
  printf 'name: fixture\n' > "$repo/.github/workflows/architecture-gates.yml"
  for tool in component-interface-snapshot rust-facade-snapshot surface-component-catalog; do
    printf '[package]\nname="%s"\n' "$tool" > "$repo/tools/$tool/Cargo.toml"
    printf 'lock\n' > "$repo/tools/$tool/Cargo.lock"
    printf 'fn main() {}\n' > "$repo/tools/$tool/src/main.rs"
  done
  printf 'toolchain\n' > "$repo/tools/surface-toolchain.env"
  chmod +x "$repo/scripts/"*.sh
  git -C "$repo" init -q
  git -C "$repo" config user.email surface@example.invalid
  git -C "$repo" config user.name SurfaceTest
  commit_case "$repo" base
}

append_entry() {
  local repo=$1 projections=$2 pr=${3:-999}
  local evidence=${4:-docs/surface-change-log.md}
  cat >> "$repo/$evidence" <<EOF

## 2026-07-30 — Fixture change ([PR #$pr](https://github.com/pablof7z/nmp/pull/$pr))

- **Failure evidence:** fixture failure.
- **Changed projections:** $projections
- **Rust / FFI / Swift / Kotlin impact:** fixture impact.
- **Persistence impact:** none.
- **Diagnostics impact:** none.
- **Updated falsifiers:** scripts/test-surface-governance.sh.
- **Superseded path removed:** fixture path.
- **Human signoff:** Fixture Reviewer, PR #$pr, 2026-07-30.
EOF
}

catalog_validate() {
  "$CATALOG_BIN" validate "$1" "${2:-HEAD}"
}

expect_fail() {
  local label=$1
  shift
  if "$@" >/dev/null 2>&1; then
    echo "FAIL: $label unexpectedly passed" >&2
    exit 1
  fi
  echo "ok - $label"
}

expect_pass() {
  local label=$1
  shift
  "$@" >/dev/null
  echo "ok - $label"
}

expect_catalog_mutation_fail() {
  local label=$1 mutate=$2
  local repo="$TMP/catalog-${label//[^a-zA-Z0-9]/-}"
  new_repo "$repo"
  eval "$mutate"
  commit_case "$repo" mutation
  expect_fail "$label" catalog_validate "$repo"
}

checker_projections() {
  SURFACE_CATALOG_BIN="$CATALOG_BIN" \
    SURFACE_ROOT="$1" SURFACE_BASE_REF="$2" SURFACE_HEAD_REF=HEAD \
    "$CHECK" --print-projections
}

run_checker() {
  local repo=$1 base=$2
  local projections
  projections=$(checker_projections "$repo" "$base")
  SURFACE_CATALOG_BIN="$CATALOG_BIN" \
  SURFACE_ROOT="$repo" \
  SURFACE_BASE_REF="$base" \
  SURFACE_HEAD_REF=HEAD \
  SURFACE_PR_NUMBER=999 \
  SURFACE_PR_URL=https://github.com/pablof7z/nmp/pull/999 \
  SURFACE_CHANGED_PROJECTIONS="$projections" \
  SURFACE_REGEN_CMD=scripts/regen.sh \
    "$CHECK"
}

# Baseline and exact root-or-omission/co-location rules.
repo="$TMP/baseline"; new_repo "$repo"
expect_pass "valid active catalog" catalog_validate "$repo"

repo="$TMP/omission"; new_repo "$repo"
sed -i.bak \
  's#^swift_manifests = .*#swift_manifests = []#; s#^swift_sources = .*#swift_sources = []#' \
  "$repo/docs/surface/components/alpha/component.toml"
rm "$repo/docs/surface/components/alpha/component.toml.bak"
printf 'swift_omission_reason = "No Swift API in this fixture."\n' \
  >> "$repo/docs/surface/components/alpha/component.toml"
commit_case "$repo" omission
expect_pass "explicit platform omission" catalog_validate "$repo"

repo="$TMP/colocated"; new_repo "$repo"; descriptor "$repo" beta alpha
commit_case "$repo" colocated
expect_pass "co-located namespace without library fields" catalog_validate "$repo"

expect_catalog_mutation_fail "missing omission reason" \
  "sed -i.bak 's#^swift_manifests = .*#swift_manifests = []#; s#^swift_sources = .*#swift_sources = []#' '$TMP/catalog-missing-omission-reason/docs/surface/components/alpha/component.toml'; rm '$TMP/catalog-missing-omission-reason/docs/surface/components/alpha/component.toml.bak'"
expect_catalog_mutation_fail "unknown descriptor field" \
  "printf 'invented = true\\n' >> '$TMP/catalog-unknown-descriptor-field/docs/surface/components/alpha/component.toml'"
expect_catalog_mutation_fail "legacy snapshot resurrection" \
  "printf 'old\\n' > '$TMP/catalog-legacy-snapshot-resurrection/docs/surface/nmp-ffi-component.txt'"
expect_catalog_mutation_fail "snapshot NUL byte" \
  "printf 'component \"alpha\"\\0namespace \"alpha_ffi\"\\n' > '$TMP/catalog-snapshot-NUL-byte/docs/surface/components/alpha/uniffi.txt'"
expect_catalog_mutation_fail "snapshot CRLF line ending" \
  "printf 'component \"alpha\"\\r\\nnamespace \"alpha_ffi\"\\r\\n' > '$TMP/catalog-snapshot-CRLF-line-ending/docs/surface/components/alpha/uniffi.txt'"
expect_catalog_mutation_fail "orphan catalog file" \
  "printf 'orphan\\n' > '$TMP/catalog-orphan-catalog-file/docs/surface/components/orphan.txt'"
expect_catalog_mutation_fail "path traversal" \
  "sed -i.bak 's#crates/alpha-ffi/src#../alpha#' '$TMP/catalog-path-traversal/docs/surface/components/alpha/component.toml'; rm '$TMP/catalog-path-traversal/docs/surface/components/alpha/component.toml.bak'"
expect_catalog_mutation_fail "missing declared source root" \
  "rm -r '$TMP/catalog-missing-declared-source-root/crates/alpha-ffi/src'"
expect_catalog_mutation_fail "manifest package mismatch" \
  "sed -i.bak 's/ffi_package = \"alpha-ffi\"/ffi_package = \"wrong-ffi\"/' '$TMP/catalog-manifest-package-mismatch/docs/surface/components/alpha/component.toml'; rm '$TMP/catalog-manifest-package-mismatch/docs/surface/components/alpha/component.toml.bak'"
expect_catalog_mutation_fail "manifest library mismatch" \
  "sed -i.bak 's/library_stem = \"alpha_ffi\"/library_stem = \"wrong_ffi\"/' '$TMP/catalog-manifest-library-mismatch/docs/surface/components/alpha/component.toml'; rm '$TMP/catalog-manifest-library-mismatch/docs/surface/components/alpha/component.toml.bak'"
expect_catalog_mutation_fail "duplicate namespace" \
  "descriptor '$TMP/catalog-duplicate-namespace' beta beta; sed -i.bak 's/beta_ffi/alpha_ffi/' '$TMP/catalog-duplicate-namespace/docs/surface/components/beta/component.toml'; rm '$TMP/catalog-duplicate-namespace/docs/surface/components/beta/component.toml.bak'"
expect_catalog_mutation_fail "duplicate component key" \
  "descriptor '$TMP/catalog-duplicate-component-key' beta beta; sed -i.bak 's/key = \"beta\"/key = \"alpha\"/' '$TMP/catalog-duplicate-component-key/docs/surface/components/beta/component.toml'; rm '$TMP/catalog-duplicate-component-key/docs/surface/components/beta/component.toml.bak'"
expect_catalog_mutation_fail "duplicate package identity" \
  "descriptor '$TMP/catalog-duplicate-package-identity' beta beta; sed -i.bak 's/beta-ffi/alpha-ffi/' '$TMP/catalog-duplicate-package-identity/docs/surface/components/beta/component.toml'; rm '$TMP/catalog-duplicate-package-identity/docs/surface/components/beta/component.toml.bak'"
expect_catalog_mutation_fail "duplicate library identity" \
  "descriptor '$TMP/catalog-duplicate-library-identity' beta beta; sed -i.bak 's/library_stem = \"beta_ffi\"/library_stem = \"alpha_ffi\"/' '$TMP/catalog-duplicate-library-identity/docs/surface/components/beta/component.toml'; rm '$TMP/catalog-duplicate-library-identity/docs/surface/components/beta/component.toml.bak'"
expect_catalog_mutation_fail "overlapping source roots" \
  "descriptor '$TMP/catalog-overlapping-source-roots' beta beta; sed -i.bak 's#crates/beta-ffi/src#crates/alpha-ffi/src#' '$TMP/catalog-overlapping-source-roots/docs/surface/components/beta/component.toml'; rm '$TMP/catalog-overlapping-source-roots/docs/surface/components/beta/component.toml.bak'"
expect_catalog_mutation_fail "co-located library fields" \
  "descriptor '$TMP/catalog-co-located-library-fields' beta alpha; printf 'ffi_package = \"beta-ffi\"\\nffi_manifest = \"crates/alpha-ffi/Cargo.toml\"\\nlibrary_stem = \"beta_ffi\"\\n' >> '$TMP/catalog-co-located-library-fields/docs/surface/components/beta/component.toml'"
expect_catalog_mutation_fail "unknown artifact owner" \
  "sed -i.bak 's/artifact_owner = \"alpha\"/artifact_owner = \"missing\"/' '$TMP/catalog-unknown-artifact-owner/docs/surface/components/alpha/component.toml'; rm '$TMP/catalog-unknown-artifact-owner/docs/surface/components/alpha/component.toml.bak'"

repo="$TMP/catalog-submodule-source-root"; new_repo "$repo"
gitlink=$(git -C "$repo" rev-parse HEAD)
git -C "$repo" rm -qr crates/alpha-ffi/src
git -C "$repo" update-index --add --cacheinfo "160000,$gitlink,crates/alpha-ffi/src"
git -C "$repo" commit -qm submodule-source-root
expect_fail "declared source root cannot be a submodule" catalog_validate "$repo"

# Git-mode and exact size ceilings (boundary passes, +1 fails).
repo="$TMP/symlink"; new_repo "$repo"
rm "$repo/docs/surface/components/alpha/component.toml"
ln -s ../../../surface-change-log.md "$repo/docs/surface/components/alpha/component.toml"
commit_case "$repo" symlink
expect_fail "descriptor symlink" catalog_validate "$repo"

repo="$TMP/descriptor-size"; new_repo "$repo"
file="$repo/docs/surface/components/alpha/component.toml"
bytes=$(wc -c < "$file" | tr -d ' ')
printf '\n#' >> "$file"
head -c "$((32768 - bytes - 2))" /dev/zero | tr '\0' x >> "$file"
commit_case "$repo" exact-descriptor
expect_pass "descriptor 32768-byte boundary" catalog_validate "$repo"
printf x >> "$file"; commit_case "$repo" descriptor-plus-one
expect_fail "descriptor 32769-byte refusal" catalog_validate "$repo"

repo="$TMP/snapshot-lines"; new_repo "$repo"
yes x | head -n 20000 > "$repo/docs/surface/components/alpha/uniffi.txt" || true
commit_case "$repo" exact-lines
expect_pass "snapshot 20000-line boundary" catalog_validate "$repo"
printf 'x\n' >> "$repo/docs/surface/components/alpha/uniffi.txt"
commit_case "$repo" lines-plus-one
expect_fail "snapshot 20001-line refusal" catalog_validate "$repo"

repo="$TMP/snapshot-bytes"; new_repo "$repo"
head -c 2000000 /dev/zero | tr '\0' x > "$repo/docs/surface/components/alpha/uniffi.txt"
commit_case "$repo" exact-bytes
expect_pass "snapshot 2000000-byte boundary" catalog_validate "$repo"
printf x >> "$repo/docs/surface/components/alpha/uniffi.txt"
commit_case "$repo" bytes-plus-one
expect_fail "snapshot 2000001-byte refusal" catalog_validate "$repo"

repo="$TMP/records"; new_repo "$repo"
for n in $(seq 2 128); do descriptor "$repo" "component-$n" alpha; done
commit_case "$repo" records-128
expect_pass "catalog 128-record boundary" catalog_validate "$repo"
descriptor "$repo" component-129 alpha
commit_case "$repo" records-129
expect_fail "catalog 129-record refusal" catalog_validate "$repo"

# Android is closed/all-required and identities are globally unique.
repo="$TMP/android"; new_repo "$repo"
mkdir -p "$repo/android/alpha/src"
printf 'android\n' > "$repo/android/alpha/build.gradle.kts"
printf 'class Alpha\n' > "$repo/android/alpha/src/Alpha.kt"
cat >> "$repo/docs/surface/components/alpha/component.toml" <<'EOF'

[android]
gradle_project = ":alpha"
namespace = "dev.nmp.alpha"
maven_coordinate = "dev.nmp:alpha"
manifests = ["android/alpha/build.gradle.kts"]
sources = ["android/alpha/src"]
EOF
commit_case "$repo" android
expect_pass "complete Android record" catalog_validate "$repo"

repo="$TMP/android-duplicate"; new_repo "$repo"; descriptor "$repo" beta beta
for key in alpha beta; do
  mkdir -p "$repo/android/$key/src"
  printf 'android\n' > "$repo/android/$key/build.gradle.kts"
  printf 'class Fixture\n' > "$repo/android/$key/src/Fixture.kt"
  cat >> "$repo/docs/surface/components/$key/component.toml" <<EOF

[android]
gradle_project = ":same"
namespace = "dev.nmp.same"
maven_coordinate = "dev.nmp:same"
manifests = ["android/$key/build.gradle.kts"]
sources = ["android/$key/src"]
EOF
done
commit_case "$repo" duplicate-android
expect_fail "duplicate Android identities" catalog_validate "$repo"

repo="$TMP/android-incomplete"; new_repo "$repo"
cat >> "$repo/docs/surface/components/alpha/component.toml" <<'EOF'

[android]
gradle_project = ":alpha"
namespace = "dev.nmp.alpha"
maven_coordinate = "dev.nmp:alpha"
manifests = []
sources = []
EOF
commit_case "$repo" incomplete-android
expect_fail "incomplete Android paths" catalog_validate "$repo"

# Transition invariants: stable identities, exact tombstones, reservations,
# path resurrection, owner/child order, and immutable retirement.
repo="$TMP/retire"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
"$CATALOG_BIN" render-tombstone "$repo" "$base" alpha 999 \
  https://github.com/pablof7z/nmp/pull/999 \
  > "$repo/docs/surface/components/alpha/component.toml"
rm -r "$repo/crates/alpha-ffi" "$repo/Packages/Alpha" "$repo/Packages/AlphaKotlin"
rm "$repo/docs/surface/components/alpha/uniffi.txt"
commit_case "$repo" retire
expect_pass "exact retirement tombstone" "$CATALOG_BIN" transition "$repo" "$base" HEAD 999 https://github.com/pablof7z/nmp/pull/999
retired=$(git -C "$repo" rev-parse HEAD)
printf '\n# mutation\n' >> "$repo/docs/surface/components/alpha/component.toml"
commit_case "$repo" mutate-tombstone
expect_fail "retired tombstone byte mutation" "$CATALOG_BIN" transition "$repo" "$retired" HEAD 999 https://github.com/pablof7z/nmp/pull/999

repo="$TMP/resurrection"; new_repo "$repo"; active=$(git -C "$repo" rev-parse HEAD)
"$CATALOG_BIN" render-tombstone "$repo" "$active" alpha 999 \
  https://github.com/pablof7z/nmp/pull/999 \
  > "$repo/docs/surface/components/alpha/component.toml"
rm -r "$repo/crates/alpha-ffi" "$repo/Packages/Alpha" "$repo/Packages/AlphaKotlin"
rm "$repo/docs/surface/components/alpha/uniffi.txt"
commit_case "$repo" retired
retired=$(git -C "$repo" rev-parse HEAD)
mkdir -p "$repo/crates/alpha-ffi/src"
printf 'resurrected\n' > "$repo/crates/alpha-ffi/src/lib.rs"
commit_case "$repo" resurrect-path
expect_fail "retired path resurrection" "$CATALOG_BIN" transition "$repo" "$retired" HEAD 999 https://github.com/pablof7z/nmp/pull/999

repo="$TMP/reserved-reuse"; new_repo "$repo"; active=$(git -C "$repo" rev-parse HEAD)
"$CATALOG_BIN" render-tombstone "$repo" "$active" alpha 999 \
  https://github.com/pablof7z/nmp/pull/999 \
  > "$repo/docs/surface/components/alpha/component.toml"
rm -r "$repo/crates/alpha-ffi" "$repo/Packages/Alpha" "$repo/Packages/AlphaKotlin"
rm "$repo/docs/surface/components/alpha/uniffi.txt"
commit_case "$repo" retired
retired=$(git -C "$repo" rev-parse HEAD)
descriptor "$repo" beta beta
sed -i.bak 's/ffi_package = "beta-ffi"/ffi_package = "alpha-ffi"/' \
  "$repo/docs/surface/components/beta/component.toml"
rm "$repo/docs/surface/components/beta/component.toml.bak"
commit_case "$repo" reuse-package
expect_fail "reserved identity reuse" "$CATALOG_BIN" transition "$repo" "$retired" HEAD 999 https://github.com/pablof7z/nmp/pull/999

repo="$TMP/reactivate"; new_repo "$repo"; active=$(git -C "$repo" rev-parse HEAD)
"$CATALOG_BIN" render-tombstone "$repo" "$active" alpha 999 \
  https://github.com/pablof7z/nmp/pull/999 \
  > "$repo/docs/surface/components/alpha/component.toml"
rm -r "$repo/crates/alpha-ffi" "$repo/Packages/Alpha" "$repo/Packages/AlphaKotlin"
rm "$repo/docs/surface/components/alpha/uniffi.txt"
commit_case "$repo" retired
retired=$(git -C "$repo" rev-parse HEAD)
for path in \
  docs/surface/components/alpha/component.toml \
  docs/surface/components/alpha/uniffi.txt \
  crates/alpha-ffi/Cargo.toml \
  crates/alpha-ffi/src/lib.rs \
  Packages/Alpha/Package.swift \
  Packages/Alpha/Sources/Alpha/Alpha.swift \
  Packages/AlphaKotlin/build.gradle.kts \
  Packages/AlphaKotlin/src/main/kotlin/Alpha.kt; do
  mkdir -p "$repo/$(dirname "$path")"
  git -C "$repo" show "$active:$path" > "$repo/$path"
done
commit_case "$repo" reactivate
expect_fail "retired component reactivation" "$CATALOG_BIN" transition "$repo" "$retired" HEAD 999 https://github.com/pablof7z/nmp/pull/999

repo="$TMP/retire-incomplete"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
"$CATALOG_BIN" render-tombstone "$repo" "$base" alpha 999 \
  https://github.com/pablof7z/nmp/pull/999 \
  > "$repo/docs/surface/components/alpha/component.toml"
rm "$repo/docs/surface/components/alpha/uniffi.txt"
commit_case "$repo" incomplete-retire
expect_fail "retirement retains derived paths" "$CATALOG_BIN" transition "$repo" "$base" HEAD 999 https://github.com/pablof7z/nmp/pull/999

repo="$TMP/retire-owner"; new_repo "$repo"; descriptor "$repo" beta alpha
commit_case "$repo" child; base=$(git -C "$repo" rev-parse HEAD)
"$CATALOG_BIN" render-tombstone "$repo" "$base" alpha 999 \
  https://github.com/pablof7z/nmp/pull/999 \
  > "$repo/docs/surface/components/alpha/component.toml"
rm -r "$repo/crates/alpha-ffi" "$repo/Packages/Alpha" "$repo/Packages/AlphaKotlin"
rm "$repo/docs/surface/components/alpha/uniffi.txt"
commit_case "$repo" retire-owner
expect_fail "owner retirement with live child" "$CATALOG_BIN" transition "$repo" "$base" HEAD 999 https://github.com/pablof7z/nmp/pull/999

repo="$TMP/package-move"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
sed -i.bak 's/ffi_package = "alpha-ffi"/ffi_package = "renamed-ffi"/' \
  "$repo/docs/surface/components/alpha/component.toml"
rm "$repo/docs/surface/components/alpha/component.toml.bak"
sed -i.bak 's/name = "alpha-ffi"/name = "renamed-ffi"/' \
  "$repo/crates/alpha-ffi/Cargo.toml"
rm "$repo/crates/alpha-ffi/Cargo.toml.bak"
printf '\n[lib]\nname = "alpha_ffi"\n' >> "$repo/crates/alpha-ffi/Cargo.toml"
commit_case "$repo" move-package
expect_pass "governed active package movement" "$CATALOG_BIN" transition "$repo" "$base" HEAD 999 https://github.com/pablof7z/nmp/pull/999

repo="$TMP/owner-move"; new_repo "$repo"; descriptor "$repo" beta beta
commit_case "$repo" second-owner
base=$(git -C "$repo" rev-parse HEAD)
sed -i.bak \
  '/^ffi_package = /d; /^ffi_manifest = /d; /^library_stem = /d; s/artifact_owner = "alpha"/artifact_owner = "beta"/' \
  "$repo/docs/surface/components/alpha/component.toml"
rm "$repo/docs/surface/components/alpha/component.toml.bak"
commit_case "$repo" move-build-owner
expect_pass "governed active build-owner movement" "$CATALOG_BIN" transition "$repo" "$base" HEAD 999 https://github.com/pablof7z/nmp/pull/999

repo="$TMP/stable-swift-root"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
mkdir -p "$repo/Packages/AlphaV2/Sources/Alpha"
printf '// package\n' > "$repo/Packages/AlphaV2/Package.swift"
printf '// swift AlphaWidget\n' > "$repo/Packages/AlphaV2/Sources/Alpha/Alpha.swift"
sed -i.bak \
  's#Packages/Alpha/Package.swift#Packages/AlphaV2/Package.swift#; s#Packages/Alpha/Sources/Alpha#Packages/AlphaV2/Sources/Alpha#' \
  "$repo/docs/surface/components/alpha/component.toml"
rm "$repo/docs/surface/components/alpha/component.toml.bak"
commit_case "$repo" move-swift-root
expect_fail "active Swift package roots are stable" "$CATALOG_BIN" transition "$repo" "$base" HEAD 999 https://github.com/pablof7z/nmp/pull/999

repo="$TMP/stable-kotlin-root"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
mkdir -p "$repo/Packages/AlphaKotlinV2/src/main/kotlin"
printf '// gradle\n' > "$repo/Packages/AlphaKotlinV2/build.gradle.kts"
printf 'class AlphaWidget\n' > "$repo/Packages/AlphaKotlinV2/src/main/kotlin/Alpha.kt"
sed -i.bak \
  's#Packages/AlphaKotlin/build.gradle.kts#Packages/AlphaKotlinV2/build.gradle.kts#; s#Packages/AlphaKotlin/src/main/kotlin#Packages/AlphaKotlinV2/src/main/kotlin#' \
  "$repo/docs/surface/components/alpha/component.toml"
rm "$repo/docs/surface/components/alpha/component.toml.bak"
commit_case "$repo" move-kotlin-root
expect_fail "active Kotlin package roots are stable" "$CATALOG_BIN" transition "$repo" "$base" HEAD 999 https://github.com/pablof7z/nmp/pull/999

repo="$TMP/delete"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
rm -r "$repo/docs/surface/components/alpha"
commit_case "$repo" delete-record
expect_fail "descriptor deletion" "$CATALOG_BIN" transition "$repo" "$base" HEAD 999 https://github.com/pablof7z/nmp/pull/999

repo="$TMP/new-retired"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
descriptor "$repo" beta beta
commit_case "$repo" beta-active
middle=$(git -C "$repo" rev-parse HEAD)
"$CATALOG_BIN" render-tombstone "$repo" "$middle" beta 999 \
  https://github.com/pablof7z/nmp/pull/999 \
  > "$repo/docs/surface/components/beta/component.toml"
rm -r "$repo/crates/beta-ffi"
rm "$repo/docs/surface/components/beta/uniffi.txt"
commit_case "$repo" beta-retired
expect_fail "new record cannot begin retired" "$CATALOG_BIN" transition "$repo" "$base" HEAD 999 https://github.com/pablof7z/nmp/pull/999

# The catalog's one-shot bootstrap transition arm (base has no catalog, head
# starts one) was deleted with #1171: absence of any base governance artifact
# must fail closed rather than let a proposed head introduce the catalog for
# the first time. This exercises `main.rs`'s `(false, true) =>
# Err(invalid("component catalog is absent from the base"))` arm directly, so
# a regression that turns it back into an `Ok(...)` bootstrap allowance is
# caught here rather than shipping unnoticed.
repo="$TMP/catalog-appears-at-head"; new_repo "$repo"
rm -r "$repo/docs/surface/components"
commit_case "$repo" catalog-absent-from-base
base=$(git -C "$repo" rev-parse HEAD)
mkdir -p "$repo/docs/surface/components"
printf '# Fixture component catalog\n' > "$repo/docs/surface/components/README.md"
descriptor "$repo" alpha alpha
commit_case "$repo" catalog-present-at-head
expect_fail "component catalog absent from base cannot silently begin at head" \
  "$CATALOG_BIN" transition "$repo" "$base" HEAD 999 https://github.com/pablof7z/nmp/pull/999

if grep -R -Fq 'SURFACE_OWNER_BOOTSTRAP' \
  "$ROOT/scripts/check-surface-governance.sh" \
  "$ROOT/scripts/run-surface-regeneration-governance.sh"; then
  fail "retired environment bootstrap bypass is still reachable"
fi
echo "ok - no environment bootstrap bypass"

# Allowlist schema is exact, canonical, and component-scoped.
allowlist_fail() {
  local label=$1 body=$2
  local repo="$TMP/allow-${label//[^a-zA-Z0-9]/-}"
  new_repo "$repo"
  printf '%s\n' "$body" > "$repo/scripts/check-sdk-parity-allowlist.toml"
  commit_case "$repo" allowlist
  expect_fail "$label" "$CATALOG_BIN" allowlist-rows "$repo" HEAD
}
allowlist_fail "allowlist unknown field" $'schema = 1\nunknown = true'
allowlist_fail "allowlist unknown component" $'schema = 1\n[[exception]]\ncomponent="missing"\nconcept="widget"\nplatform="swift"\njustification="fixture"'
allowlist_fail "allowlist malformed concept" $'schema = 1\n[[exception]]\ncomponent="alpha"\nconcept="Bad-Word"\nplatform="swift"\njustification="fixture"'
allowlist_fail "allowlist malformed platform" $'schema = 1\n[[exception]]\ncomponent="alpha"\nconcept="widget"\nplatform="rust"\njustification="fixture"'
allowlist_fail "allowlist empty reason" $'schema = 1\n[[exception]]\ncomponent="alpha"\nconcept="widget"\nplatform="swift"\njustification=" "'
allowlist_fail "allowlist duplicate tuple" $'schema = 1\n[[exception]]\ncomponent="alpha"\nconcept="widget"\nplatform="swift"\njustification="one"\n[[exception]]\ncomponent="alpha"\nconcept="widget"\nplatform="swift"\njustification="two"'
allowlist_fail "allowlist noncanonical order" $'schema = 1\n[[exception]]\ncomponent="alpha"\nconcept="widget"\nplatform="swift"\njustification="swift first"\n[[exception]]\ncomponent="alpha"\nconcept="widget"\nplatform="kotlin"\njustification="kotlin belongs first"'

# A concept in beta cannot mask alpha's absent Swift concept.
repo="$TMP/parity"; new_repo "$repo"; descriptor "$repo" beta beta
mkdir -p "$repo/Packages/Beta/Sources/Beta" "$repo/Packages/BetaKotlin/src/main/kotlin"
printf '// package\n' > "$repo/Packages/Beta/Package.swift"
printf '// Widget BetaThing\n' > "$repo/Packages/Beta/Sources/Beta/Beta.swift"
printf '// gradle\n' > "$repo/Packages/BetaKotlin/build.gradle.kts"
printf 'class BetaThing\n' > "$repo/Packages/BetaKotlin/src/main/kotlin/Beta.kt"
cat > "$repo/crates/beta-ffi/src/lib.rs" <<'EOF'
#[derive(uniffi::Record)]
pub struct FfiBetaThing {
    pub value: String,
}
EOF
cat > "$repo/docs/surface/components/beta/component.toml" <<'EOF'
schema = 1
key = "beta"
state = "active"
uniffi_namespace = "beta_ffi"
artifact_owner = "beta"
ffi_package = "beta-ffi"
ffi_manifest = "crates/beta-ffi/Cargo.toml"
library_stem = "beta_ffi"
ffi_sources = ["crates/beta-ffi/src"]
swift_manifests = ["Packages/Beta/Package.swift"]
swift_sources = ["Packages/Beta/Sources/Beta"]
kotlin_manifests = ["Packages/BetaKotlin/build.gradle.kts"]
kotlin_sources = ["Packages/BetaKotlin/src/main/kotlin"]
EOF
printf '// Alpha only.\n' \
  > "$repo/Packages/Alpha/Sources/Alpha/Alpha.swift"
commit_case "$repo" parity-cross-mask
expect_fail "cross-component parity masking" env \
  SDK_PARITY_ROOT="$repo" SDK_PARITY_HEAD_REF=HEAD \
  SDK_PARITY_CATALOG_BIN="$CATALOG_BIN" bash "$PARITY" --quiet
cat > "$repo/scripts/check-sdk-parity-allowlist.toml" <<'EOF'
schema = 1
[[exception]]
component = "alpha"
concept = "widget"
platform = "swift"
justification = "Fixture deliberately omits alpha's Swift widget."
EOF
commit_case "$repo" exact-exception
expect_pass "exact component parity exception" env \
  SDK_PARITY_ROOT="$repo" SDK_PARITY_HEAD_REF=HEAD \
  SDK_PARITY_CATALOG_BIN="$CATALOG_BIN" bash "$PARITY" --quiet
printf '// swift AlphaWidget\n' \
  > "$repo/Packages/Alpha/Sources/Alpha/Alpha.swift"
commit_case "$repo" stale-exception
stale_output=$(
  SDK_PARITY_ROOT="$repo" SDK_PARITY_HEAD_REF=HEAD \
    SDK_PARITY_CATALOG_BIN="$CATALOG_BIN" bash "$PARITY"
)
grep -Fq 'CURRENTLY-UNUSED ALLOWLIST ENTRIES FOR SWIFT (alpha)' \
  <<< "$stale_output" || {
  echo "FAIL: stale parity exception was not visible" >&2
  exit 1
}
echo "ok - stale parity exception is visible"

# Retiring one component cannot make parity for the survivors vacuous.
repo="$TMP/parity-after-removal"; new_repo "$repo"; descriptor "$repo" beta beta
commit_case "$repo" second-component
active=$(git -C "$repo" rev-parse HEAD)
"$CATALOG_BIN" render-tombstone "$repo" "$active" beta 999 \
  https://github.com/pablof7z/nmp/pull/999 \
  > "$repo/docs/surface/components/beta/component.toml"
rm -r "$repo/crates/beta-ffi"
rm "$repo/docs/surface/components/beta/uniffi.txt"
commit_case "$repo" retire-beta
expect_pass "component removal preserves remaining parity" env \
  SDK_PARITY_ROOT="$repo" SDK_PARITY_HEAD_REF=HEAD \
  SDK_PARITY_CATALOG_BIN="$CATALOG_BIN" bash "$PARITY" --quiet
printf '// Alpha wrapper removed.\n' \
  > "$repo/Packages/Alpha/Sources/Alpha/Alpha.swift"
commit_case "$repo" break-surviving-component
expect_fail "component removal cannot mask surviving parity failure" env \
  SDK_PARITY_ROOT="$repo" SDK_PARITY_HEAD_REF=HEAD \
  SDK_PARITY_CATALOG_BIN="$CATALOG_BIN" bash "$PARITY" --quiet

# End-to-end checker: regeneration, evidence, projections, protected program,
# bootstrap refusal in steady state, and dirty checkout behavior.
repo="$TMP/checker-valid"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
printf 'component "alpha"\nnamespace "alpha_ffi"\nrecord "v2"\n' \
  > "$repo/actual/components/alpha/uniffi.txt"
cp "$repo/actual/components/alpha/uniffi.txt" \
  "$repo/docs/surface/components/alpha/uniffi.txt"
append_entry "$repo" ffi
commit_case "$repo" valid-change
expect_pass "complete governed change" run_checker "$repo" "$base"

repo="$TMP/checker-stale"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
printf 'stale delta\n' >> "$repo/actual/components/alpha/uniffi.txt"
append_entry "$repo" ffi
commit_case "$repo" stale
expect_fail "stale component snapshot" run_checker "$repo" "$base"

repo="$TMP/checker-no-log"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
printf '// changed\n' >> "$repo/Packages/Alpha/Sources/Alpha/Alpha.swift"
commit_case "$repo" no-log
expect_fail "governed SDK change without evidence" run_checker "$repo" "$base"

repo="$TMP/checker-swift-manifest"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
printf '// changed Swift manifest\n' >> "$repo/Packages/Alpha/Package.swift"
append_entry "$repo" swift
commit_case "$repo" swift-manifest
expect_pass "Swift-only manifest evidence" run_checker "$repo" "$base"

repo="$TMP/checker-kotlin-manifest"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
printf '// changed Kotlin manifest\n' >> "$repo/Packages/AlphaKotlin/build.gradle.kts"
append_entry "$repo" kotlin
commit_case "$repo" kotlin-manifest
expect_pass "Kotlin-only manifest evidence" run_checker "$repo" "$base"

repo="$TMP/checker-both-manifest"; new_repo "$repo"
sed -i.bak \
  's#Packages/AlphaKotlin/build.gradle.kts#Packages/Alpha/Package.swift#' \
  "$repo/docs/surface/components/alpha/component.toml"
rm "$repo/docs/surface/components/alpha/component.toml.bak"
commit_case "$repo" shared-manifest-base
base=$(git -C "$repo" rev-parse HEAD)
printf '// changed shared manifest\n' >> "$repo/Packages/Alpha/Package.swift"
append_entry "$repo" kotlin,swift
commit_case "$repo" shared-manifest
expect_pass "shared Swift/Kotlin manifest evidence" run_checker "$repo" "$base"

repo="$TMP/checker-history"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
sed -i.bak 's/seed/rewritten/' "$repo/docs/surface-change-log.md"
rm "$repo/docs/surface-change-log.md.bak"
printf 'component "alpha"\nnamespace "alpha_ffi"\nrecord "v2"\n' \
  > "$repo/actual/components/alpha/uniffi.txt"
cp "$repo/actual/components/alpha/uniffi.txt" \
  "$repo/docs/surface/components/alpha/uniffi.txt"
append_entry "$repo" ffi
commit_case "$repo" history
expect_fail "append-only history rewrite" run_checker "$repo" "$base"

repo="$TMP/checker-configured-evidence"; new_repo "$repo"
mv "$repo/docs/surface-change-log.md" "$repo/docs/surface-evidence.md"
commit_case "$repo" configured-evidence-base
base=$(git -C "$repo" rev-parse HEAD)
append_entry "$repo" correction 999 docs/surface-evidence.md
commit_case "$repo" configured-evidence-append
[[ $("$CATALOG_BIN" projections "$repo" "$base" HEAD) == none ]] || {
  echo "FAIL: catalog tool inferred an evidence path" >&2
  exit 1
}
projections=$(
  SURFACE_CATALOG_BIN="$CATALOG_BIN" \
    SURFACE_ROOT="$repo" SURFACE_BASE_REF="$base" SURFACE_HEAD_REF=HEAD \
    SURFACE_CHANGE_LOG=docs/surface-evidence.md \
    "$CHECK" --print-projections
)
[[ $projections == correction ]] || {
  echo "FAIL: configured evidence path was not recognized" >&2
  exit 1
}
SURFACE_CATALOG_BIN="$CATALOG_BIN" \
SURFACE_ROOT="$repo" \
SURFACE_BASE_REF="$base" \
SURFACE_HEAD_REF=HEAD \
SURFACE_CHANGE_LOG=docs/surface-evidence.md \
SURFACE_PR_NUMBER=999 \
SURFACE_PR_URL=https://github.com/pablof7z/nmp/pull/999 \
SURFACE_CHANGED_PROJECTIONS=correction \
SURFACE_REGEN_CMD=scripts/regen.sh \
  "$CHECK" >/dev/null
echo "ok - configured evidence path owns correction recognition"

for protected in \
  scripts/check-surface-migration-authorization.py \
  scripts/check-surface-governance.sh \
  scripts/check-sdk-parity.sh \
  scripts/lib/require-commands.sh \
  scripts/run-surface-regeneration-governance.sh \
  scripts/test-surface-migration-authorization.py \
  .github/workflows/architecture-gates.yml \
  .github/workflows/ci.yml \
  .github/workflows/surface-governance.yml \
  tools/component-interface-snapshot/Cargo.lock \
  tools/component-interface-snapshot/Cargo.toml \
  tools/component-interface-snapshot/src/main.rs \
  tools/surface-component-catalog/src/main.rs; do
  repo="$TMP/checker-protected-$(basename "$protected")"
  new_repo "$repo"
  mkdir -p "$repo/$(dirname "$protected")"
  if [[ ! -f "$repo/$protected" ]]; then
    printf 'fixture\n' > "$repo/$protected"
    commit_case "$repo" protected-base
  fi
  base=$(git -C "$repo" rev-parse HEAD)
  printf '# tamper\n' >> "$repo/$protected"
  commit_case "$repo" tamper
  expect_fail "protected program tamper: $protected" run_checker "$repo" "$base"
done

expected_permissions=$'contents:read\nissues:read\npull-requests:read\nstatuses:read'
for workflow in \
  "$ROOT/.github/workflows/surface-governance.yml" \
  "$ROOT/.github/workflows/ci.yml"; do
  [[ $(workflow_permissions "$workflow") == "$expected_permissions" ]] ||
    fail "trusted workflow permissions are not the exact least-read set: $workflow"
  grep -Fq 'check-surface-migration-authorization.py' "$workflow" ||
    fail "trusted workflow does not extract the base migration verifier: $workflow"
  grep -Fq 'test-surface-migration-authorization.py' "$workflow" ||
    fail "trusted workflow does not extract the base migration falsifier: $workflow"
  grep -Fq 'SURFACE_PR_RECORD:' "$workflow" ||
    fail "trusted workflow does not pass its PR API record: $workflow"
  grep -Fq 'SURFACE_ISSUE_RECORD:' "$workflow" ||
    fail "trusted workflow does not pass its issue API record: $workflow"
  grep -Fq 'SURFACE_STATUS_RECORDS:' "$workflow" ||
    fail "trusted workflow does not pass its status API record: $workflow"
  grep -Fq 'git show "$BASE_SHA:$path" > "$TRUSTED_DIR/$path"' "$workflow" ||
    fail "trusted workflow does not extract governance bytes from the base: $workflow"
  if grep -Eq \
      'bootstrap uses proposed copy|#954 bootstrap|cp "\$path" "\$TRUSTED_DIR/\$path"|cat-file -e "\$BASE_SHA:tools/surface-component-catalog/Cargo.toml"' \
      "$workflow"; then
    fail "trusted workflow retains a proposed-head governance fallback: $workflow"
  fi
done
falsify_missing_base_governance_artifact
if grep -Fq 'migration_candidate' "$ROOT/scripts/check-surface-governance.sh"; then
  fail "shell wrapper duplicates the verifier's migration activation authority"
fi
grep -Fq 'python3 "$MIGRATION_CHECK" "${migration_args[@]}" verify' \
  "$ROOT/scripts/check-surface-governance.sh" ||
  fail "shell wrapper does not invoke the base-owned verifier unconditionally"
echo "ok - workflows use least-read permissions and base-only governance bytes"

repo="$TMP/checker-wrong-pr"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
printf 'component "alpha"\nnamespace "alpha_ffi"\nrecord "v2"\n' \
  > "$repo/actual/components/alpha/uniffi.txt"
cp "$repo/actual/components/alpha/uniffi.txt" \
  "$repo/docs/surface/components/alpha/uniffi.txt"
append_entry "$repo" ffi 998
commit_case "$repo" wrong-pr-evidence
expect_fail "change-log evidence names wrong PR" run_checker "$repo" "$base"

repo="$TMP/checker-wrong-projection"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
printf 'component "alpha"\nnamespace "alpha_ffi"\nrecord "v2"\n' \
  > "$repo/actual/components/alpha/uniffi.txt"
cp "$repo/actual/components/alpha/uniffi.txt" \
  "$repo/docs/surface/components/alpha/uniffi.txt"
append_entry "$repo" correction
commit_case "$repo" wrong-projection-evidence
expect_fail "change-log evidence names wrong projection" run_checker "$repo" "$base"

repo="$TMP/checker-placeholder"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
printf 'component "alpha"\nnamespace "alpha_ffi"\nrecord "v2"\n' \
  > "$repo/actual/components/alpha/uniffi.txt"
cp "$repo/actual/components/alpha/uniffi.txt" \
  "$repo/docs/surface/components/alpha/uniffi.txt"
append_entry "$repo" ffi
sed -i.bak 's/Fixture Reviewer, PR #999, 2026-07-30/pending, PR #999, 2026-07-30/' \
  "$repo/docs/surface-change-log.md"
rm "$repo/docs/surface-change-log.md.bak"
commit_case "$repo" placeholder-signoff
expect_fail "change-log placeholder signoff" run_checker "$repo" "$base"

repo="$TMP/checker-dirty"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
printf 'dirty\n' >> "$repo/actual/components/alpha/uniffi.txt"
expect_fail "unstaged dirty checkout" run_checker "$repo" "$base"

repo="$TMP/checker-staged"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
printf '// staged\n' >> "$repo/Packages/Alpha/Sources/Alpha/Alpha.swift"
git -C "$repo" add Packages/Alpha/Sources/Alpha/Alpha.swift
expect_fail "staged dirty checkout" run_checker "$repo" "$base"

repo="$TMP/checker-untracked"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
printf 'untracked\n' > "$repo/docs/surface/components/untracked.txt"
expect_fail "untracked dirty checkout" run_checker "$repo" "$base"

echo "surface governance adversarial tests passed"
