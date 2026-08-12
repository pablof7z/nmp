#!/usr/bin/env bash
set -euo pipefail

# This suite is part of the base-trusted program, so it has the same two roots
# the program it proves has (#1186): PROGRAM_ROOT is where the program lives,
# ROOT (the argument) is the tree under judgment. Anything this suite builds,
# sources, or runs comes from PROGRAM_ROOT. Reading the tree under judgment for
# a tool is the defect, and this suite must not commit it either -- it used to,
# and a proposed head with `exit 0` in tools/surface-toolchain.env made the
# whole suite exit 0 having asserted almost nothing.
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
PROGRAM_ROOT=$(cd "$SCRIPT_DIR/.." && pwd)
REPORT="$SCRIPT_DIR/report-surface-governance-verdict.sh"
PARITY="$SCRIPT_DIR/check-sdk-parity.sh"
# The checker's three-way exit contract (#1264). A suite that only asserted
# "nonzero" could not tell a verdict about the head from the gate breaking --
# which is the exact confusion the reporting split exists to remove, so the
# suite has to be able to see it too.
CHECK_STALE_BASE_EXIT=4
CHECK_MALFUNCTION_EXIT=70
[[ $# -eq 1 ]] || {
  echo "usage: $0 <workspace-root>" >&2
  exit 2
}
ROOT=$1
git -C "$ROOT" rev-parse --show-toplevel >/dev/null 2>&1 || {
  echo "test-surface-governance: workspace root is not a Git worktree: $ROOT" >&2
  exit 2
}
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

# Every line of one job, so an assertion can say "this appears in that job and
# nowhere else". Job identifiers are the only two-space keys under `jobs:`;
# `on:` children share that indentation but never share a job's name.
workflow_job_lines() {
  awk -v want="$2" '
    /^  [A-Za-z0-9_-]+:[[:space:]]*$/ {
      job = $0
      sub(/^  /, "", job)
      sub(/:[[:space:]]*$/, "", job)
      next
    }
    job == want { print }
  ' "$1"
}

# The workflow-level `defaults: run: shell:` value, or empty if the file does
# not declare one. A step-level `shell:` would appear more deeply indented and
# is deliberately not matched: the assertion below is about the one shell every
# step inherits.
workflow_default_shell() {
  awk '
    /^defaults:[[:space:]]*$/ { defaults = 1; next }
    /^[^[:space:]#]/ { defaults = 0; run = 0 }
    defaults && /^  run:[[:space:]]*$/ { run = 1; next }
    defaults && /^  [A-Za-z0-9_-]+:/ { run = 0 }
    run && /^    shell:[[:space:]]/ {
      line = $0
      sub(/^    shell:[[:space:]]*/, "", line)
      print line
    }
  ' "$1"
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
  source "$PROGRAM_ROOT/tools/surface-toolchain.env"
  target=${SURFACE_CATALOG_TARGET_DIR:-$TMP/catalog-target}
  cargo "+$SURFACE_RUST_TOOLCHAIN" build --quiet --locked \
    --manifest-path "$PROGRAM_ROOT/tools/surface-component-catalog/Cargo.toml" \
    --target-dir "$target"
  CATALOG_BIN="$target/debug/nmp-surface-component-catalog"
fi
[[ -x "$CATALOG_BIN" ]]

# The checker under test runs out of its own scratch program directory, the way
# CI runs it, so that "the program resolves its tools from where it lives"
# is a property this suite can actually observe. A regenerator that compiles
# Rust is not something a fixture can run, so the program directory carries a
# fixture regenerator -- which is the only substitution, and it is made by
# building a program directory rather than by handing the checker a path.
PROGRAM="$TMP/program"
mkdir -p "$PROGRAM/scripts/lib" "$PROGRAM/tools"
for program_file in \
  check-surface-governance.sh \
  check-sdk-parity.sh \
  report-surface-governance-verdict.sh \
  run-surface-regeneration-governance.sh; do
  cp "$SCRIPT_DIR/$program_file" "$PROGRAM/scripts/$program_file"
done
cp "$SCRIPT_DIR/lib/require-commands.sh" "$PROGRAM/scripts/lib/"
cp "$PROGRAM_ROOT/tools/surface-toolchain.env" "$PROGRAM/tools/"
cat > "$PROGRAM/scripts/regenerate-surface-snapshots.sh" <<'REGEN'
#!/usr/bin/env bash
set -euo pipefail
[[ $1 == --output-dir && $# == 2 ]]
root=$(git rev-parse --show-toplevel)
mkdir -p "$2"
cp -R "$root/actual/." "$2"
REGEN
chmod +x "$PROGRAM/scripts/"*.sh
CHECK="$PROGRAM/scripts/check-surface-governance.sh"

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

# A verdict about the proposed head: exit 1, a `surface-governance:` line, and
# the exact reason the case was built to provoke. Asserting the reason is what
# stops a case from passing because something unrelated broke.
expect_verdict() {
  local label=$1 reason=$2
  shift 2
  local output status=0
  output=$("$@" 2>&1) || status=$?
  if (( status != 1 )); then
    echo "FAIL: $label exited $status; a verdict is exit 1" >&2
    printf '%s\n' "$output" >&2
    exit 1
  fi
  if ! printf '%s\n' "$output" | grep -Fq "surface-governance: $reason"; then
    echo "FAIL: $label did not render the verdict line: $reason" >&2
    printf '%s\n' "$output" >&2
    exit 1
  fi
  if printf '%s\n' "$output" | grep -Fq "surface-governance-malfunction:"; then
    echo "FAIL: $label reported a malfunction under a verdict" >&2
    printf '%s\n' "$output" >&2
    exit 1
  fi
  echo "ok - $label"
}

# A stale head. Its own exit code so the reporting layer can give it its own
# check name instead of borrowing the verdict's.
expect_stale_base() {
  local label=$1 reason=$2
  shift 2
  local output status=0
  output=$("$@" 2>&1) || status=$?
  if (( status != CHECK_STALE_BASE_EXIT )); then
    echo "FAIL: $label exited $status; staleness is exit $CHECK_STALE_BASE_EXIT" >&2
    printf '%s\n' "$output" >&2
    exit 1
  fi
  if ! printf '%s\n' "$output" | grep -Fq "$reason"; then
    echo "FAIL: $label did not report staleness: $reason" >&2
    printf '%s\n' "$output" >&2
    exit 1
  fi
  echo "ok - $label"
}

# The gate never reached a verdict. Distinct exit code, distinct prefix, and no
# verdict line at all -- a reader must not be able to mistake it for one.
expect_malfunction() {
  local label=$1 reason=$2
  shift 2
  local output status=0
  output=$("$@" 2>&1) || status=$?
  if (( status != CHECK_MALFUNCTION_EXIT )); then
    echo "FAIL: $label exited $status; a malfunction is exit $CHECK_MALFUNCTION_EXIT" >&2
    printf '%s\n' "$output" >&2
    exit 1
  fi
  if ! printf '%s\n' "$output" | grep -Fq "malfunction: $reason"; then
    echo "FAIL: $label did not name the malfunction: $reason" >&2
    printf '%s\n' "$output" >&2
    exit 1
  fi
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
repo="$TMP/android-first-declaration"; new_repo "$repo"
base=$(git -C "$repo" rev-parse HEAD)
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
commit_case "$repo" declare-android
expect_pass "first Android projection declaration" \
  "$CATALOG_BIN" transition "$repo" "$base" HEAD 999 https://github.com/pablof7z/nmp/pull/999

repo="$TMP/android-identity-change"; new_repo "$repo"
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
commit_case "$repo" declare-android
base=$(git -C "$repo" rev-parse HEAD)
sed -i.bak 's/maven_coordinate = "dev.nmp:alpha"/maven_coordinate = "dev.nmp:renamed"/' \
  "$repo/docs/surface/components/alpha/component.toml"
rm "$repo/docs/surface/components/alpha/component.toml.bak"
commit_case "$repo" change-android-identity
expect_fail "declared Android package identities are stable" \
  "$CATALOG_BIN" transition "$repo" "$base" HEAD 999 https://github.com/pablof7z/nmp/pull/999

repo="$TMP/android-removal"; new_repo "$repo"
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
commit_case "$repo" declare-android
base=$(git -C "$repo" rev-parse HEAD)
sed -i.bak '/^\[android\]$/,$d' \
  "$repo/docs/surface/components/alpha/component.toml"
rm "$repo/docs/surface/components/alpha/component.toml.bak"
commit_case "$repo" remove-android
expect_fail "declared Android projection cannot disappear" \
  "$CATALOG_BIN" transition "$repo" "$base" HEAD 999 https://github.com/pablof7z/nmp/pull/999

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

# End-to-end checker: regeneration, evidence, projections, base-trusted program,
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
expect_verdict "stale component snapshot" \
  "docs/surface/components/alpha/uniffi.txt is stale; regenerate and commit it" \
  run_checker "$repo" "$base"

repo="$TMP/checker-no-log"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
printf '// changed\n' >> "$repo/Packages/Alpha/Sources/Alpha/Alpha.swift"
commit_case "$repo" no-log
expect_verdict "governed SDK change without evidence" \
  "governed projection changed without an appended change-log entry" \
  run_checker "$repo" "$base"

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
expect_verdict "append-only history rewrite" \
  "historical change-log content was edited, deleted, or reordered" \
  run_checker "$repo" "$base"

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
  "$CHECK" >/dev/null
echo "ok - configured evidence path owns correction recognition"

expected_permissions='contents:read'
for entry in \
  "$ROOT/.github/workflows/surface-governance.yml:surface-governance" \
  "$ROOT/.github/workflows/ci.yml:surface-regeneration"; do
  workflow=${entry%:*}
  gate=${entry##*:}
  [[ $(workflow_permissions "$workflow") == "$expected_permissions" ]] ||
    fail "trusted workflow permissions are not the exact least-read set: $workflow"
  if grep -Eq 'surface-migration-authorization|SURFACE_(PR|ISSUE|STATUS)_RECORD' "$workflow"; then
    fail "workflow retains deleted protected-path authorization plumbing: $workflow"
  fi
  grep -Fq 'git show "$BASE_SHA:$path" > "$TRUSTED_DIR/$path"' "$workflow" ||
    fail "trusted workflow does not extract governance bytes from the base: $workflow"
  if grep -Eq \
      'bootstrap uses proposed copy|#954 bootstrap|cp "\$path" "\$TRUSTED_DIR/\$path"|cat-file -e "\$BASE_SHA:tools/surface-component-catalog/Cargo.toml"' \
      "$workflow"; then
    fail "trusted workflow retains a proposed-head governance fallback: $workflow"
  fi

  # #1264: one check name per claim. Undoing the split -- folding the self-test
  # back in with the verdict, or letting the verdict-named job run the gate
  # itself -- fails here, which is what makes the separation live rather than
  # incidental.
  selftest_job=$(workflow_job_lines "$workflow" "$gate-selftest")
  verdict_job=$(workflow_job_lines "$workflow" "$gate-verdict-rendered")
  base_job=$(workflow_job_lines "$workflow" "$gate-current-base")
  render_job=$(workflow_job_lines "$workflow" "$gate")
  for named in "$gate-selftest:$selftest_job" "$gate-verdict-rendered:$verdict_job" \
    "$gate-current-base:$base_job" "$gate:$render_job"; do
    [[ -n ${named#*:} ]] ||
      fail "trusted workflow has no ${named%%:*} job: $workflow"
  done
  printf '%s\n' "$selftest_job" |
    grep -Eq 'runner\.temp.*scripts/test-surface-governance\.sh' ||
    fail "the self-test job does not run the governance falsifiers: $workflow"
  printf '%s\n' "$selftest_job" |
    grep -Eq 'runner\.temp.*scripts/test-install-surface-tools\.sh' ||
    fail "the self-test job does not run the installer falsifiers: $workflow"
  if printf '%s\n' "$verdict_job" |
      grep -Eq 'runner\.temp.*scripts/test-(surface-governance|install-surface-tools)\.sh'; then
    fail "the verdict job still runs the gate's own falsifiers: $workflow"
  fi
  printf '%s\n' "$verdict_job" |
    grep -Eq 'runner\.temp.*scripts/report-surface-governance-verdict\.sh' ||
    fail "the verdict job does not report through the outcome reporter: $workflow"
  if printf '%s\n' "$render_job" | grep -Eq 'runner\.temp.*scripts/'; then
    fail "the verdict-named job runs the gate instead of reading its outcome: $workflow"
  fi
  for reader in "$base_job" "$render_job"; do
    printf '%s\n' "$reader" |
      grep -Fq "needs.$gate-verdict-rendered.outputs.outcome" ||
      fail "a reporting job does not read the rendered outcome: $workflow"
  done

  # Every extraction block in the file must extract the same set. The two jobs
  # each need the whole base-trusted program, and a list that drifts would let
  # one job silently resolve a tool from the head (#1186).
  blocks=$(grep -c 'git show "\$BASE_SHA:\$path" > "\$TRUSTED_DIR/\$path"' "$workflow")
  (( blocks >= 2 )) ||
    fail "trusted workflow no longer extracts the base program per job: $workflow"
  while read -r count path; do
    (( count == blocks )) ||
      fail "extracted path is missing from an extraction block: $path ($workflow)"
  done < <(
    grep -E '^[[:space:]]+(scripts|tools)/[^[:space:]]+[[:space:]]*\\?$' "$workflow" |
      awk '{ gsub(/[[:space:]]/, ""); sub(/\\$/, ""); print }' |
      LC_ALL=C sort | uniq -c
  )
  grep -Fq 'scripts/report-surface-governance-verdict.sh' "$workflow" ||
    fail "trusted workflow does not extract the base outcome reporter: $workflow"
  # Extraction must fail closed and say why. A workflow that shrugged off a
  # missing base artifact -- or filled it from the head -- would be the hole
  # #1186 already names, and would make deleting the reporter a silent pass.
  grep -Fq "the head's copy is deliberately NOT used instead" "$workflow" ||
    fail "trusted workflow does not name a missing base artifact as a malfunction: $workflow"

  # #1186: extracting the program from the base is only worth something if the
  # program then runs the extracted copy. Every one of these named a path the
  # base-trusted checker would execute, source, or build, and every one of them
  # silently resolved to the proposed head when the workflow forgot to set it --
  # which surface-governance.yml did, for the toolchain file the checker
  # sources. The program now finds its own tooling, so a workflow that hands it
  # a program path is reintroducing the hole rather than plugging it.
  if grep -Eq \
      '^[[:space:]]+SURFACE_(TOOLCHAIN_ENV|REGEN_CMD|CATALOG_TOOL_DIR|COMPONENT_TOOL_DIR|RUST_FACADE_TOOL_DIR|CHECKER|CATALOG_BIN):' \
      "$workflow"; then
    fail "trusted workflow hands the gate a program path instead of letting it resolve its own: $workflow"
  fi
done

# #1170: a green step is only evidence if the command the workflow names is the
# command that ran. GitHub's default `run:` shell is an ordinary non-interactive
# bash, which reads $BASH_ENV, imports shell functions from the environment, and
# honours profile files -- so a step can exit 0 having executed something else
# entirely while the workflow text still reads `cargo test --workspace`. Every
# workflow therefore declares one hardened shell for every step it runs.
#
# The string is read back out of the workflows rather than assumed, and the
# behaviour below is proved with the string that was read. Weakening the
# declaration in a workflow therefore fails here twice: once for not matching
# the other workflows, and once because the weakened flags no longer defeat the
# bypass.
declared_shell=""
shopt -s nullglob
proposed_workflows=("$ROOT"/.github/workflows/*.yml)
shopt -u nullglob
(( ${#proposed_workflows[@]} >= 2 )) ||
  fail "the proposed head has no workflows to check"
for workflow in "${proposed_workflows[@]}"; do
  workflow_shell=$(workflow_default_shell "$workflow")
  [[ -n $workflow_shell ]] ||
    fail "workflow does not declare a hardened shell for its steps: $workflow"
  if [[ -z $declared_shell ]]; then
    declared_shell=$workflow_shell
  elif [[ $workflow_shell != "$declared_shell" ]]; then
    fail "workflows disagree on the hardened shell: $workflow"
  fi
  # Exactly the one declaration, so no job or step can opt back out of it.
  (( $(grep -c '^[[:space:]]*shell:' "$workflow") == 1 )) ||
    fail "a job or step overrides the workflow's hardened shell: $workflow"
done
[[ $declared_shell == *" {0}" ]] ||
  fail "the declared shell is not a step-script invocation: $declared_shell"

# The probe is a real executable that records that it ran and then fails, so
# "the named command ran" and "a real failure is still reported" are the same
# observation (#1170 falsifier 4).
probe_dir="$TMP/shell-hardening"
mkdir -p "$probe_dir/bin"
probe_witness="$probe_dir/probe-ran"
cat > "$probe_dir/bin/nmp_gate_probe" <<EOF
#!/usr/bin/env bash
touch "$probe_witness"
exit 3
EOF
chmod +x "$probe_dir/bin/nmp_gate_probe"
printf 'nmp_gate_probe\n' > "$probe_dir/step.sh"
printf 'nmp_gate_probe() { return 0; }\n' > "$probe_dir/shadow.sh"

run_step() {
  # $1: "hardened" or "default"; the rest is the environment to inject.
  local mode=$1
  shift
  rm -f "$probe_witness"
  local -a argv
  if [[ $mode == hardened ]]; then
    # The declared string with {0} replaced by the step script, exactly as the
    # runner expands it.
    read -r -a argv <<< "${declared_shell/\{0\}/$probe_dir/step.sh}"
  else
    argv=(bash -e "$probe_dir/step.sh")
  fi
  local status=0
  env PATH="$probe_dir/bin:$PATH" "$@" "${argv[@]}" >/dev/null 2>&1 || status=$?
  printf '%s\n' "$status"
}

# The bypass is real: assert it before asserting it is closed, so this pair
# cannot pass because the probe was broken.
[[ $(run_step default BASH_ENV="$probe_dir/shadow.sh") == 0 ]] ||
  fail "the BASH_ENV bypass did not reproduce; this falsifier proves nothing"
[[ ! -e $probe_witness ]] ||
  fail "the BASH_ENV bypass ran the real program; this falsifier proves nothing"
[[ $(run_step hardened BASH_ENV="$probe_dir/shadow.sh") == 3 ]] ||
  fail "the declared shell still honours BASH_ENV: $declared_shell"
[[ -e $probe_witness ]] ||
  fail "the declared shell did not run the command the step names: $declared_shell"

exported_shadow() {
  # An exported function, the second half of #1170: a step inherits it without
  # any file being involved.
  nmp_gate_probe() { return 0; }
  export -f nmp_gate_probe
  run_step "$1"
}
[[ $(exported_shadow default) == 0 ]] ||
  fail "the inherited-function bypass did not reproduce; this falsifier proves nothing"
[[ $(exported_shadow hardened) == 3 ]] ||
  fail "the declared shell still inherits shell functions: $declared_shell"
unset -f nmp_gate_probe 2>/dev/null || true
echo "ok - every workflow step runs the command its workflow names"

# The reporter must survive in the proposed tree, not only in the base.
# Otherwise a head could delete it while the extraction line still found the
# base's surviving copy, and master would end up with workflows whose program is
# gone -- green all the way down.
[[ -x "$ROOT/scripts/report-surface-governance-verdict.sh" ]] ||
  fail "the proposed head has no executable outcome reporter"

# For the same reason, the claims this suite makes have to survive in the
# proposed tree. A head that keeps a mechanism but deletes its proof passes --
# correctly, the head is sound -- and then becomes the base, at which point the
# mechanism can be removed with nothing left to fail. The list below makes the
# remaining base-trust claims self-checking.
#
# Every needle below is BUILT, never written out whole, because this file is
# the file being searched: a list of literal needles matches itself, so
# deleting the thing it guards leaves the list behind to satisfy it. That is
# not hypothetical -- the first version of this block was written with literal
# needles and a head that deleted both falsifier sections still passed.
for surviving_claim in \
  'ok - every workflow step runs the command its workflow names' \
  "ok - the gate runs its own tooling and never the head's" \
  'ok - workflows use least-read permissions and base-only governance bytes'; do
  grep -Fq "echo \"$surviving_claim\"" "$ROOT/scripts/test-surface-governance.sh" ||
    fail "the proposed falsifier suite no longer claims: $surviving_claim"
done
# The claim lines alone could be kept while their assertions were deleted, so
# the machinery each one summarises has to be present too.
for surviving_definition in \
  workflow_default_shell \
  falsify_head_supplied_tooling_is_never_run; do
  grep -Fq "$surviving_definition() {" "$ROOT/scripts/test-surface-governance.sh" ||
    fail "the proposed falsifier suite no longer defines $surviving_definition"
done

# #1186, at the source: the proposed head's program must not resolve anything
# it runs from the tree it is judging. `git show` and `$ROOT/docs/...` are data
# reads and stay; a program path under the tree under judgment does not.
for program in \
  scripts/check-surface-governance.sh \
  scripts/regenerate-surface-snapshots.sh \
  scripts/install-surface-tools.sh \
  scripts/run-surface-regeneration-governance.sh; do
  proposed="$ROOT/$program"
  [[ -f $proposed ]] || fail "the proposed head has no $program"
  if grep -Eq '\$\{?(ROOT|SURFACE_ROOT)[^}]*\}?/(tools|scripts)/' "$proposed"; then
    fail "$program resolves a program from the tree it is judging"
  fi
  if grep -Eq \
      'SURFACE_(TOOLCHAIN_ENV|REGEN_CMD|CATALOG_TOOL_DIR|COMPONENT_TOOL_DIR|RUST_FACADE_TOOL_DIR|CHECKER)' \
      "$proposed"; then
    fail "$program takes a program path from its caller again"
  fi
done
grep -Fq 'PROGRAM_ROOT=$(cd "$SCRIPT_DIR/.." && pwd)' \
  "$ROOT/scripts/check-surface-governance.sh" ||
  fail "the proposed checker does not resolve its tooling from where it lives"

# The installer prepares the gate and says nothing about a proposed head, so
# every way it fails is a malfunction and has to exit 70 under its own prefix.
# Its own falsifiers run the BASE copy, which is the right claim for them to
# make and the wrong one for judging a head, so the head's copy is checked
# here. Before this, a fatal git inside it escaped as exit 128 -- a step that
# could not run the command it names, reported as neither a verdict nor a
# legible malfunction.
grep -Fq 'MALFUNCTION_EXIT=70' "$ROOT/scripts/install-surface-tools.sh" ||
  fail "the proposed installer does not report its failures as gate malfunctions"
grep -Fq "surface-tools-malfunction:" "$ROOT/scripts/install-surface-tools.sh" ||
  fail "the proposed installer does not name its failures as gate malfunctions"

# The falsifier suites are part of the same base-trusted program and had the
# same defect: this suite used to source the tree under judgment's toolchain
# definition, so a head with `exit 0` in it made the whole suite exit 0 having
# asserted almost nothing -- a self-test reporting success without running its
# command. They read the tree under judgment on purpose, so their rule is
# narrower than the one above: nothing under it may be sourced or built.
for program in \
  scripts/test-surface-governance.sh \
  scripts/test-install-surface-tools.sh; do
  proposed="$ROOT/$program"
  [[ -f $proposed ]] || fail "the proposed head has no $program"
  if grep -Eq '(^|[[:space:]])(source|\.)[[:space:]]+"?\$\{?(ROOT|SURFACE_ROOT)' "$proposed"; then
    fail "$program sources the tree it is judging"
  fi
  if grep -Eq -- '--manifest-path[[:space:]]+"?\$\{?(ROOT|SURFACE_ROOT)' "$proposed"; then
    fail "$program builds a tool out of the tree it is judging"
  fi
  if grep -Eq \
      'SURFACE_(TOOLCHAIN_ENV|REGEN_CMD|CATALOG_TOOL_DIR|COMPONENT_TOOL_DIR|RUST_FACADE_TOOL_DIR|CHECKER)' \
      "$proposed"; then
    fail "$program takes a program path from its caller again"
  fi
done

# And in behaviour. The head below carries its own copies of two things the
# gate runs -- the toolchain definition the checker sources, and the
# regenerator it executes -- and each one records that it ran. Neither may
# fire, and the gate must not accept the head on the strength of them: before
# this was fixed, `exit 0` in the head's tools/surface-toolchain.env made the
# base-trusted checker exit 0, which the outcome reporter reads as `accepted`.
falsify_head_supplied_tooling_is_never_run() {
  local repo="$TMP/head-tooling"
  local sourced="$TMP/head-tooling-sourced"
  local executed="$TMP/head-tooling-executed"
  local program="$TMP/head-tooling-program"
  new_repo "$repo"
  local base
  base=$(git -C "$repo" rev-parse HEAD)
  cat > "$repo/tools/surface-toolchain.env" <<EOF
touch "$sourced"
exit 0
EOF
  cat > "$repo/scripts/regenerate-surface-snapshots.sh" <<EOF
#!/usr/bin/env bash
touch "$executed"
exit 0
EOF
  chmod +x "$repo/scripts/regenerate-surface-snapshots.sh"
  commit_case "$repo" head-supplied-tooling

  # A program directory that is not the head: its toolchain definition names a
  # toolchain that does not exist, so if the checker sources its own copy the
  # catalog build fails and the gate reports a malfunction. If it sources the
  # head's copy instead, the gate exits 0 and the witness appears.
  mkdir -p "$program/scripts/lib" "$program/tools/surface-component-catalog"
  cp "$SCRIPT_DIR/check-surface-governance.sh" "$program/scripts/"
  cp "$PROGRAM/scripts/regenerate-surface-snapshots.sh" "$program/scripts/"
  chmod +x "$program/scripts/"*.sh
  printf 'SURFACE_RUST_TOOLCHAIN=nmp-gate-fixture-toolchain\n' \
    > "$program/tools/surface-toolchain.env"

  local status=0
  env \
    SURFACE_ROOT="$repo" \
    SURFACE_BASE_REF="$base" \
    SURFACE_HEAD_REF=HEAD \
    SURFACE_PR_NUMBER=999 \
    SURFACE_PR_URL=https://github.com/pablof7z/nmp/pull/999 \
    SURFACE_CHANGED_PROJECTIONS=none \
    SURFACE_CATALOG_TARGET_DIR="$TMP/head-tooling-target" \
    "$program/scripts/check-surface-governance.sh" >/dev/null 2>&1 || status=$?
  [[ ! -e $sourced ]] ||
    fail "the head's tools/surface-toolchain.env was sourced by the base-trusted gate"
  (( status != 0 )) ||
    fail "the gate accepted a head whose own tooling decided the outcome"

  # Now with a working catalog binary, so the run reaches regeneration and the
  # regenerator the head supplied is the one that would be executed.
  status=0
  env \
    SURFACE_CATALOG_BIN="$CATALOG_BIN" \
    SURFACE_ROOT="$repo" \
    SURFACE_BASE_REF="$base" \
    SURFACE_HEAD_REF=HEAD \
    SURFACE_PR_NUMBER=999 \
    SURFACE_PR_URL=https://github.com/pablof7z/nmp/pull/999 \
    SURFACE_CHANGED_PROJECTIONS=none \
    "$program/scripts/check-surface-governance.sh" >/dev/null 2>&1 || status=$?
  [[ ! -e $executed ]] ||
    fail "the head's scripts/regenerate-surface-snapshots.sh was executed by the base-trusted gate"
  [[ ! -e $sourced ]] ||
    fail "the head's tools/surface-toolchain.env was sourced by the base-trusted gate"
}
falsify_head_supplied_tooling_is_never_run
echo "ok - the gate runs its own tooling and never the head's"

falsify_missing_base_governance_artifact
echo "ok - workflows use least-read permissions and base-only governance bytes"

repo="$TMP/checker-wrong-pr"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
printf 'component "alpha"\nnamespace "alpha_ffi"\nrecord "v2"\n' \
  > "$repo/actual/components/alpha/uniffi.txt"
cp "$repo/actual/components/alpha/uniffi.txt" \
  "$repo/docs/surface/components/alpha/uniffi.txt"
append_entry "$repo" ffi 998
commit_case "$repo" wrong-pr-evidence
expect_verdict "change-log evidence names wrong PR" \
  "appended entry 1 must link this exact PR: https://github.com/pablof7z/nmp/pull/999" \
  run_checker "$repo" "$base"

repo="$TMP/checker-wrong-projection"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
printf 'component "alpha"\nnamespace "alpha_ffi"\nrecord "v2"\n' \
  > "$repo/actual/components/alpha/uniffi.txt"
cp "$repo/actual/components/alpha/uniffi.txt" \
  "$repo/docs/surface/components/alpha/uniffi.txt"
append_entry "$repo" correction
commit_case "$repo" wrong-projection-evidence
expect_verdict "change-log evidence names wrong projection" \
  "appended entry 1 projections must be exactly: ffi" \
  run_checker "$repo" "$base"

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
expect_verdict "change-log placeholder signoff" \
  "appended entry 1 has a placeholder human signoff" \
  run_checker "$repo" "$base"

repo="$TMP/checker-dirty"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
printf 'dirty\n' >> "$repo/actual/components/alpha/uniffi.txt"
expect_malfunction "unstaged dirty checkout" \
  "deterministic regeneration requires a clean worktree" \
  run_checker "$repo" "$base"

repo="$TMP/checker-staged"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
printf '// staged\n' >> "$repo/Packages/Alpha/Sources/Alpha/Alpha.swift"
git -C "$repo" add Packages/Alpha/Sources/Alpha/Alpha.swift
expect_malfunction "staged dirty checkout" \
  "deterministic regeneration requires a clean worktree" \
  run_checker "$repo" "$base"

repo="$TMP/checker-untracked"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
printf 'untracked\n' > "$repo/docs/surface/components/untracked.txt"
expect_malfunction "untracked dirty checkout" \
  "deterministic regeneration requires a clean worktree" \
  run_checker "$repo" "$base"

# #1264: a malfunction must be visibly a malfunction and never a verdict, in
# both directions. The cases below break the gate on purpose -- its tool, its
# scratch state, its wiring, and the gate process itself -- and each one has to
# come out with the malfunction exit code and prefix rather than a verdict line.

repo="$TMP/checker-for-malfunction"; new_repo "$repo"
base=$(git -C "$repo" rev-parse HEAD)
printf '# tamper\n' >> "$repo/.github/workflows/ci.yml"
commit_case "$repo" ordinary-change
fixture_repo=$repo
fixture_base=$base

run_checker_with() {
  # run_checker, but with the named environment overridden, so one induced
  # break at a time is the only difference from the verdict case above.
  local repo=$1 base=$2
  shift 2
  local projections
  projections=$(checker_projections "$repo" "$base")
  env \
    SURFACE_CATALOG_BIN="$CATALOG_BIN" \
    SURFACE_ROOT="$repo" \
    SURFACE_BASE_REF="$base" \
    SURFACE_HEAD_REF=HEAD \
    SURFACE_PR_NUMBER=999 \
    SURFACE_PR_URL=https://github.com/pablof7z/nmp/pull/999 \
    SURFACE_CHANGED_PROJECTIONS="$projections" \
    "$@" \
    "$CHECK"
}

expect_malfunction "the gate's tool is missing" \
  "component catalog tool is unavailable" \
  run_checker_with "$fixture_repo" "$fixture_base" \
  SURFACE_CATALOG_BIN="$TMP/no-such-catalog-tool"

expect_malfunction "the gate is wired to a commit that does not exist" \
  "base commit is unavailable" \
  run_checker_with "$fixture_repo" "$fixture_base" \
  SURFACE_BASE_REF=0000000000000000000000000000000000000000

# Staleness: the head is not on the current base, so nothing about it was
# judged. Distinct exit code, distinct line, distinct check name in CI.
repo="$TMP/checker-stale-base"; new_repo "$repo"
root_commit=$(git -C "$repo" rev-parse HEAD)
git -C "$repo" checkout -q -b advanced-base "$root_commit"
printf 'the base moved on\n' > "$repo/ordinary.txt"
commit_case "$repo" advance-base
advanced_base=$(git -C "$repo" rev-parse HEAD)
git -C "$repo" checkout -q -b proposed-head "$root_commit"
printf '# tamper\n' >> "$repo/.github/workflows/ci.yml"
commit_case "$repo" change-on-stale-branch
expect_stale_base "head is not descended from the current PR base" \
  "head is not descended from the current PR base" \
  run_checker "$repo" "$advanced_base"

# The reporter is what turns those exit codes into a check name. Its mapping is
# proved directly, including the case the workflow cannot express: a gate that
# is killed and never exits on its own terms at all.
expect_report() {
  local label=$1 expected_outcome=$2 expected_status=$3
  shift 3
  local output status=0
  output=$("$@" 2>&1) || status=$?
  if (( status != expected_status )); then
    echo "FAIL: $label reported exit $status, expected $expected_status" >&2
    printf '%s\n' "$output" >&2
    exit 1
  fi
  if ! printf '%s\n' "$output" |
      grep -Fq "surface-governance-outcome: $expected_outcome"; then
    echo "FAIL: $label did not report outcome $expected_outcome" >&2
    printf '%s\n' "$output" >&2
    exit 1
  fi
  echo "ok - $label"
}

exiting_gate() {
  local script="$TMP/gate-exit-$1.sh"
  printf '#!/usr/bin/env bash\nexit %s\n' "$1" > "$script"
  chmod +x "$script"
  printf '%s\n' "$script"
}

expect_report "reporter: an accepted head is a verdict" accepted 0 \
  "$REPORT" "$(exiting_gate 0)"
expect_report "reporter: a rejected head is a verdict" rejected 0 \
  "$REPORT" "$(exiting_gate 1)"
expect_report "reporter: a stale head is a verdict" stale-base 0 \
  "$REPORT" "$(exiting_gate $CHECK_STALE_BASE_EXIT)"
expect_report "reporter: a malfunction is not a verdict" \
  no-verdict "$CHECK_MALFUNCTION_EXIT" \
  "$REPORT" "$(exiting_gate $CHECK_MALFUNCTION_EXIT)"
expect_report "reporter: an unclassified exit is not a verdict" \
  no-verdict "$CHECK_MALFUNCTION_EXIT" \
  "$REPORT" "$(exiting_gate 2)"

killed_gate="$TMP/gate-killed.sh"
cat > "$killed_gate" <<'GATE'
#!/usr/bin/env bash
kill -KILL $$
GATE
chmod +x "$killed_gate"
expect_report "reporter: a gate killed mid-run is not a verdict" \
  no-verdict "$CHECK_MALFUNCTION_EXIT" \
  "$REPORT" "$killed_gate"

expect_report "reporter: nothing to run is not a verdict" \
  no-verdict "$CHECK_MALFUNCTION_EXIT" \
  "$REPORT" "$TMP/no-such-gate-program"

# End to end through the real checker: one fixture, one reporter, two induced
# situations, two different reports.
real_gate() {
  # A file, not an exported shell function: the reporter runs whatever program
  # it is handed, and handing it a function would make this suite depend on the
  # inherited-environment behaviour that #1170 is about.
  local name=$1 catalog=$2 script="$TMP/real-gate-$1.sh"
  cat > "$script" <<GATE
#!/usr/bin/env bash
exec env \\
  SURFACE_CATALOG_BIN="$catalog" \\
  SURFACE_ROOT="$fixture_repo" \\
  SURFACE_BASE_REF="$fixture_base" \\
  SURFACE_HEAD_REF=HEAD \\
  SURFACE_PR_NUMBER=999 \\
  SURFACE_PR_URL=https://github.com/pablof7z/nmp/pull/999 \\
  SURFACE_CHANGED_PROJECTIONS=none \\
  SURFACE_SKIP_REGEN=1 \\
  "$CHECK"
GATE
  chmod +x "$script"
  printf '%s\n' "$script"
}
expect_report "reporter: the real gate accepting an ordinary head" accepted 0 \
  "$REPORT" "$(real_gate sound "$CATALOG_BIN")"
expect_report "reporter: the real gate broken on the same head" \
  no-verdict "$CHECK_MALFUNCTION_EXIT" \
  "$REPORT" "$(real_gate broken "$TMP/no-such-catalog-tool")"

echo "surface governance adversarial tests passed"
