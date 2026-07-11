#!/usr/bin/env bash
set -euo pipefail

CHECK=$(cd "$(dirname "$0")" && pwd)/check-surface-governance.sh
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

new_repo() {
  local repo=$1
  mkdir -p \
    "$repo/.github/workflows" \
    "$repo/docs/surface" \
    "$repo/scripts" \
    "$repo/tools" \
    "$repo/tools/component-interface-snapshot/src" \
    "$repo/actual" \
    "$repo/crates/nmp-ffi/src" \
    "$repo/Packages/NMP/Sources/NMP" \
    "$repo/Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk"
  cp "$CHECK" "$repo/scripts/check-surface-governance.sh"
  for path in install-surface-tools.sh regenerate-surface-snapshots.sh \
    test-install-surface-tools.sh test-surface-governance.sh; do
    printf '#!/usr/bin/env bash\nexit 0\n' > "$repo/scripts/$path"
  done
  printf 'name: trusted fixture\n' > "$repo/.github/workflows/surface-governance.yml"
  printf 'name: ordinary fixture\n' > "$repo/.github/workflows/ci.yml"
  printf 'fixture pins\n' > "$repo/tools/surface-toolchain.env"
  printf '[package]\nname="fixture-extractor"\n' > "$repo/tools/component-interface-snapshot/Cargo.toml"
  printf 'fixture lock\n' > "$repo/tools/component-interface-snapshot/Cargo.lock"
  cat > "$repo/tools/component-interface-snapshot/src/main.rs" <<'EXTRACTOR'
#!/usr/bin/env bash
set -euo pipefail
cat "$1/crates/nmp-ffi/src/types.rs" > "$2"
EXTRACTOR
  printf 'ffi-v1\n' > "$repo/crates/nmp-ffi/src/types.rs"
  printf '// swift package v1\n' > "$repo/Packages/NMP/Package.swift"
  printf '// kotlin build v1\n' > "$repo/Packages/NMPKotlin/build.gradle.kts"
  printf '// kotlin settings v1\n' > "$repo/Packages/NMPKotlin/settings.gradle.kts"
  printf 'facade-v1\n' > "$repo/actual/nmp-facade.txt"
  printf 'ffi-v1\n' > "$repo/actual/nmp-ffi-component.txt"
  cp "$repo/actual/"* "$repo/docs/surface/"
  cat > "$repo/docs/surface-change-log.md" <<'LOG'
# Surface change log

Historical bytes below are append-only.

## Historical entry A

seed A

## Historical entry B

seed B
LOG
  cat > "$repo/scripts/regen.sh" <<'REGEN'
#!/usr/bin/env bash
set -euo pipefail
[[ $1 == --output-dir && $# -eq 2 ]]
mkdir -p "$2"
cp actual/nmp-facade.txt actual/nmp-ffi-component.txt "$2/"
REGEN
  chmod +x "$repo/scripts/"*.sh
  git -C "$repo" init -q
  git -C "$repo" config user.email surface@example.invalid
  git -C "$repo" config user.name SurfaceTest
  git -C "$repo" add .
  git -C "$repo" commit -qm base
}

fixture_projections() {
  SURFACE_ROOT=$1 SURFACE_BASE_REF=$2 SURFACE_HEAD_REF=HEAD \
    "$CHECK" --print-projections
}

run_check() {
  local repo=$1 base=$2 projections=${3:-}
  [[ -n "$projections" ]] || projections=$(fixture_projections "$repo" "$base")
  SURFACE_ROOT="$repo" \
  SURFACE_BASE_REF="$base" \
  SURFACE_HEAD_REF=HEAD \
  SURFACE_PR_NUMBER=999 \
  SURFACE_PR_URL=https://github.com/pablof7z/nmp/pull/999 \
  SURFACE_CHANGED_PROJECTIONS="$projections" \
  SURFACE_REGEN_CMD=scripts/regen.sh \
  "$CHECK"
}

expect_fail() {
  local label=$1 repo=$2 base=$3 projections=${4:-}
  if run_check "$repo" "$base" "$projections" >/dev/null 2>&1; then
    echo "FAIL: $label unexpectedly passed" >&2
    exit 1
  fi
  echo "ok - $label"
}

append_valid_entry() {
  local repo=$1 projections=$2 pr=${3:-999}
  cat >> "$repo/docs/surface-change-log.md" <<ENTRY

## 2026-07-11 — Test surface change ([PR #$pr](https://github.com/pablof7z/nmp/pull/$pr))

- **Failure evidence:** test fixture proves the old shape fails.
- **Changed projections:** $projections
- **Rust / FFI / Swift / Kotlin impact:** affected projections are exercised; others are unchanged.
- **Persistence impact:** none.
- **Diagnostics impact:** none.
- **Updated falsifiers:** scripts/test-surface-governance.sh.
- **Superseded path removed:** old test shape removed.
- **Human signoff:** Test Reviewer, PR #$pr, 2026-07-11.
ENTRY
}

commit_case() { git -C "$1" add . && git -C "$1" commit -qm "$2"; }

# Derived Rust output changed but the committed snapshot did not.
repo="$TMP/rust-no-snapshot"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
printf 'facade-v2\n' > "$repo/actual/nmp-facade.txt"; commit_case "$repo" rust-only
expect_fail "Rust surface change without snapshot" "$repo" "$base"

# Snapshot moved without its append-only evidence entry.
repo="$TMP/snapshot-no-log"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
printf 'facade-v2\n' > "$repo/actual/nmp-facade.txt"; cp "$repo/actual/nmp-facade.txt" "$repo/docs/surface/"
commit_case "$repo" snapshot-only
expect_fail "snapshot update without appended entry" "$repo" "$base"

# Both native ergonomic wrapper directories are governed even without a
# generated snapshot delta.
for projection in swift kotlin; do
  repo="$TMP/$projection-no-log"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
  if [[ $projection == swift ]]; then
    printf 'public struct NewSurface {}\n' > "$repo/Packages/NMP/Sources/NMP/NewSurface.swift"
  else
    printf 'class NewSurface\n' > "$repo/Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/NewSurface.kt"
  fi
  commit_case "$repo" "$projection-wrapper-only"
  expect_fail "$projection wrapper change without log" "$repo" "$base"
done

# Consumer-visible package manifests are governed even when wrapper source and
# generated snapshots do not move.
for manifest in Packages/NMP/Package.swift \
  Packages/NMPKotlin/build.gradle.kts Packages/NMPKotlin/settings.gradle.kts; do
  repo="$TMP/manifest-$(basename "$manifest")"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
  printf '\n// public package change\n' >> "$repo/$manifest"; commit_case "$repo" manifest-only
  expect_fail "package manifest change without log: $manifest" "$repo" "$base"
done

# Historical edits, deletion, and reorder all violate exact-prefix history.
repo="$TMP/history-edit"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
printf 'facade-v2\n' > "$repo/actual/nmp-facade.txt"; cp "$repo/actual/nmp-facade.txt" "$repo/docs/surface/"
sed -i.bak 's/seed A/rewritten A/' "$repo/docs/surface-change-log.md"; rm "$repo/docs/surface-change-log.md.bak"
append_valid_entry "$repo" rust; commit_case "$repo" history-edit
expect_fail "historical log rewrite" "$repo" "$base"

repo="$TMP/history-delete"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
printf 'facade-v2\n' > "$repo/actual/nmp-facade.txt"; cp "$repo/actual/nmp-facade.txt" "$repo/docs/surface/"
sed -i.bak '/seed A/d' "$repo/docs/surface-change-log.md"; rm "$repo/docs/surface-change-log.md.bak"
append_valid_entry "$repo" rust; commit_case "$repo" history-delete
expect_fail "historical log deletion" "$repo" "$base"

repo="$TMP/history-reorder"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
printf 'facade-v2\n' > "$repo/actual/nmp-facade.txt"; cp "$repo/actual/nmp-facade.txt" "$repo/docs/surface/"
cat > "$repo/docs/surface-change-log.md" <<'REORDERED'
# Surface change log

Historical bytes below are append-only.

## Historical entry B

seed B

## Historical entry A

seed A
REORDERED
append_valid_entry "$repo" rust; commit_case "$repo" history-reorder
expect_fail "historical log reorder" "$repo" "$base"

# Empty required evidence cannot satisfy the schema.
repo="$TMP/empty-field"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
printf 'facade-v2\n' > "$repo/actual/nmp-facade.txt"; cp "$repo/actual/nmp-facade.txt" "$repo/docs/surface/"
append_valid_entry "$repo" rust
sed -i.bak 's/- \*\*Diagnostics impact:\*\* none\./- **Diagnostics impact:** /' "$repo/docs/surface-change-log.md"; rm "$repo/docs/surface-change-log.md.bak"
commit_case "$repo" empty-field
expect_fail "empty required field" "$repo" "$base"

# A non-empty placeholder is still not human signoff.
repo="$TMP/pending-signoff"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
printf 'facade-v2\n' > "$repo/actual/nmp-facade.txt"; cp "$repo/actual/nmp-facade.txt" "$repo/docs/surface/"
append_valid_entry "$repo" rust
sed -i.bak 's/Test Reviewer, PR #999, 2026-07-11/pending review on PR #999/' "$repo/docs/surface-change-log.md"; rm "$repo/docs/surface-change-log.md.bak"
commit_case "$repo" pending-signoff
expect_fail "placeholder human signoff" "$repo" "$base"

# Exact runtime PR context is mandatory; a different/unrelated link fails
# without any network lookup.
repo="$TMP/wrong-pr"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
printf 'facade-v2\n' > "$repo/actual/nmp-facade.txt"; cp "$repo/actual/nmp-facade.txt" "$repo/docs/surface/"
append_valid_entry "$repo" rust 998; commit_case "$repo" wrong-pr
expect_fail "wrong or unrelated PR link" "$repo" "$base"

# The trusted base checker rejects attempts to replace itself or its workflow.
for protected in scripts/check-surface-governance.sh .github/workflows/ci.yml \
  .github/workflows/surface-governance.yml; do
  repo="$TMP/tamper-$(basename "$protected")"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
  printf '\n# bypass attempt\n' >> "$repo/$protected"; commit_case "$repo" protected-tamper
  expect_fail "protected governance tamper: $protected" "$repo" "$base"
done
if SURFACE_BOOTSTRAP=1 run_check "$repo" "$base" >/dev/null 2>&1; then
  echo "FAIL: bootstrap flag bypassed an established trusted base" >&2
  exit 1
fi
echo "ok - bootstrap flag cannot bypass an established base"

# Even if a head changes the public FFI and replaces its extractor with a
# program that prints the old baseline, the extractor recovered from base sees
# the new interface data and the protected-tool change is rejected.
repo="$TMP/extractor-bypass"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
printf 'ffi-v2\n' > "$repo/crates/nmp-ffi/src/types.rs"
printf 'ffi-v2\n' > "$repo/actual/nmp-ffi-component.txt"
cat > "$repo/tools/component-interface-snapshot/src/main.rs" <<'BYPASS'
#!/usr/bin/env bash
printf 'ffi-v1\n' > "$2"
BYPASS
commit_case "$repo" extractor-bypass
git -C "$repo" show "$base:tools/component-interface-snapshot/src/main.rs" > "$TMP/trusted-extractor"
chmod +x "$TMP/trusted-extractor"
"$TMP/trusted-extractor" "$repo" "$TMP/trusted-ffi-output"
grep -Fxq 'ffi-v2' "$TMP/trusted-ffi-output"
expect_fail "head extractor cannot hide public FFI delta" "$repo" "$base"

# Wrapper + log (without snapshot movement) is valid.
repo="$TMP/wrapper-valid"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
printf 'public struct NewSurface {}\n' > "$repo/Packages/NMP/Sources/NMP/NewSurface.swift"
printf 'class NewSurface\n' > "$repo/Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/NewSurface.kt"
append_valid_entry "$repo" kotlin,swift; commit_case "$repo" wrapper-valid
run_check "$repo" "$base" >/dev/null
echo "ok - wrapper change plus valid append"

# Valid manifest-only changes pass with the exact affected projection entry.
repo="$TMP/swift-manifest-valid"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
printf '\n// public package change\n' >> "$repo/Packages/NMP/Package.swift"
append_valid_entry "$repo" swift; commit_case "$repo" swift-manifest-valid
run_check "$repo" "$base" >/dev/null
echo "ok - Swift manifest plus valid append"

repo="$TMP/kotlin-manifest-valid"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
printf '\n// public build change\n' >> "$repo/Packages/NMPKotlin/build.gradle.kts"
printf '\n// public settings change\n' >> "$repo/Packages/NMPKotlin/settings.gradle.kts"
append_valid_entry "$repo" kotlin; commit_case "$repo" kotlin-manifest-valid
run_check "$repo" "$base" >/dev/null
echo "ok - Kotlin manifests plus valid append"

# A correction-only append is allowed and explicitly identified.
repo="$TMP/correction-valid"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
append_valid_entry "$repo" correction; commit_case "$repo" correction-valid
run_check "$repo" "$base" >/dev/null
echo "ok - correction-only append"

# Truthful snapshot plus a complete, context-matched append is valid.
repo="$TMP/snapshot-valid"; new_repo "$repo"; base=$(git -C "$repo" rev-parse HEAD)
printf 'facade-v2\n' > "$repo/actual/nmp-facade.txt"; cp "$repo/actual/nmp-facade.txt" "$repo/docs/surface/"
append_valid_entry "$repo" rust; commit_case "$repo" snapshot-valid
run_check "$repo" "$base" >/dev/null
echo "ok - snapshot plus valid append"
