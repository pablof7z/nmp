#!/usr/bin/env bash
set -euo pipefail

ROOT=${SURFACE_ROOT:-$(git rev-parse --show-toplevel)}
BASE_REF=${SURFACE_BASE_REF:-}
HEAD_REF=${SURFACE_HEAD_REF:-HEAD}
SNAPSHOT_DIR=${SURFACE_SNAPSHOT_DIR:-docs/surface}
CHANGE_LOG=${SURFACE_CHANGE_LOG:-docs/surface-change-log.md}
REGEN_CMD=${SURFACE_REGEN_CMD:-scripts/regenerate-surface-snapshots.sh}
CATALOG_TOOL_DIR=${SURFACE_CATALOG_TOOL_DIR:-$ROOT/tools/surface-component-catalog}

fail() { echo "surface-governance: $*" >&2; exit 1; }

cd "$ROOT"
[[ -n "$BASE_REF" ]] || fail "SURFACE_BASE_REF is required"
git cat-file -e "$BASE_REF^{commit}" 2>/dev/null || fail "base commit is unavailable: $BASE_REF"
git cat-file -e "$HEAD_REF^{commit}" 2>/dev/null || fail "head commit is unavailable: $HEAD_REF"

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

CATALOG_BIN=${SURFACE_CATALOG_BIN:-}
if [[ -z "$CATALOG_BIN" ]]; then
  # shellcheck disable=SC1091
  source "${SURFACE_TOOLCHAIN_ENV:-$ROOT/tools/surface-toolchain.env}"
  CATALOG_TARGET=${SURFACE_CATALOG_TARGET_DIR:-$TMP/catalog-tool-target}
  cargo "+$SURFACE_RUST_TOOLCHAIN" build --quiet --locked \
    --manifest-path "$CATALOG_TOOL_DIR/Cargo.toml" \
    --target-dir "$CATALOG_TARGET"
  CATALOG_BIN="$CATALOG_TARGET/debug/nmp-surface-component-catalog"
fi
[[ -x "$CATALOG_BIN" ]] || fail "component catalog tool is unavailable: $CATALOG_BIN"

projection_set() {
  "$CATALOG_BIN" projections "$ROOT" "$BASE_REF" "$HEAD_REF"
}

if [[ ${1:-} == "--print-projections" ]]; then
  [[ $# -eq 1 ]] || fail "usage: $0 [--print-projections]"
  projection_set
  exit 0
elif [[ $# -ne 0 ]]; then
  fail "usage: $0 [--print-projections]"
fi

PR_NUMBER=${SURFACE_PR_NUMBER:-}
PR_URL=${SURFACE_PR_URL:-}
PASSED_PROJECTIONS=${SURFACE_CHANGED_PROJECTIONS:-}
[[ $PR_NUMBER =~ ^[1-9][0-9]*$ ]] || fail "SURFACE_PR_NUMBER must be the actual numeric PR"
EXPECTED_PR_URL="https://github.com/pablof7z/nmp/pull/$PR_NUMBER"
[[ $PR_URL == "$EXPECTED_PR_URL" ]] || fail "SURFACE_PR_URL does not match PR number"
EXPECTED_PROJECTIONS=$(projection_set)
[[ $PASSED_PROJECTIONS == "$EXPECTED_PROJECTIONS" ]] ||
  fail "changed projection context mismatch: expected $EXPECTED_PROJECTIONS, got ${PASSED_PROJECTIONS:-<empty>}"

transition_mode=$(
  "$CATALOG_BIN" transition "$ROOT" "$BASE_REF" "$HEAD_REF" "$PR_NUMBER" "$PR_URL"
)
[[ $transition_mode == bootstrap || $transition_mode == steady ]] ||
  fail "component catalog returned an unknown transition mode: $transition_mode"

# Parse the untrusted Git filename stream without line delimiters. Rename/copy
# statuses carry two paths; every other status carries one.
git diff --name-status -z "$BASE_REF...$HEAD_REF" > "$TMP/name-status"
exec 3< "$TMP/name-status"
: > "$TMP/changed-paths"
while IFS= read -r -d '' status <&3; do
  IFS= read -r -d '' path <&3 || fail "truncated git diff after status $status"
  printf '%s\0' "$path" >> "$TMP/changed-paths"
  case "$status" in
    R*|C*)
      IFS= read -r -d '' path <&3 || fail "truncated git rename/copy after status $status"
      printf '%s\0' "$path" >> "$TMP/changed-paths"
      ;;
  esac
done
exec 3<&-

# The pull_request_target workflow and these scripts are trusted from the base
# revision. A PR cannot replace the program that judges itself. The sole
# exception is this exact legacy-snapshot -> two-record owner bootstrap, and
# only when the owner procedure explicitly sets SURFACE_OWNER_BOOTSTRAP=1.
owner_bootstrap=0
if [[ ${SURFACE_OWNER_BOOTSTRAP:-0} == 1 && $transition_mode == bootstrap ]]; then
  owner_bootstrap=1
fi
while IFS= read -r -d '' path; do
  case "$path" in
    .github/workflows/architecture-gates.yml|\
    .github/workflows/ci.yml|\
    .github/workflows/surface-governance.yml|\
    scripts/check-sdk-parity.sh|\
    scripts/check-sdk-parity-allowlist.txt|\
    scripts/check-sdk-parity-allowlist.toml|\
    scripts/check-surface-governance.sh|\
    scripts/install-surface-tools.sh|\
    scripts/lib/require-commands.sh|\
    scripts/regenerate-surface-snapshots.sh|\
    scripts/test-install-surface-tools.sh|\
    scripts/test-surface-governance.sh|\
    tools/component-interface-snapshot/Cargo.lock|\
    tools/component-interface-snapshot/Cargo.toml|\
    tools/component-interface-snapshot/src/main.rs|\
    tools/rust-facade-snapshot/Cargo.lock|\
    tools/rust-facade-snapshot/Cargo.toml|\
    tools/rust-facade-snapshot/src/main.rs|\
    tools/rust-facade-snapshot/tests/fixtures/*|\
    tools/surface-component-catalog/Cargo.lock|\
    tools/surface-component-catalog/Cargo.toml|\
    tools/surface-component-catalog/src/main.rs|\
    tools/surface-toolchain.env)
      [[ $owner_bootstrap == 1 ]] ||
        fail "protected governance program changed: $path"
      ;;
  esac
done < "$TMP/changed-paths"

if [[ ${SURFACE_OWNER_BOOTSTRAP:-0} == 1 && $owner_bootstrap != 1 ]]; then
  fail "SURFACE_OWNER_BOOTSTRAP applies only to the exact legacy catalog bootstrap"
fi

# The ordinary pull_request job runs deterministic regeneration with the same
# protected checker/CI program. The pull_request_target job sets SKIP_REGEN and
# treats the untrusted head strictly as Git data; it never compiles head code.
if [[ ${SURFACE_SKIP_REGEN:-0} != 1 ]]; then
  SURFACE_HEAD_REF="$HEAD_REF" \
    SURFACE_CATALOG_BIN="$CATALOG_BIN" \
    "$REGEN_CMD" --output-dir "$TMP/generated"

  git show "$HEAD_REF:$SNAPSHOT_DIR/nmp-facade.txt" > "$TMP/committed-facade" 2>/dev/null ||
    fail "head snapshot is unavailable: $SNAPSHOT_DIR/nmp-facade.txt"
  cmp -s "$TMP/committed-facade" "$TMP/generated/nmp-facade.txt" || {
    diff -u "$TMP/committed-facade" "$TMP/generated/nmp-facade.txt" >&2 || true
    fail "$SNAPSHOT_DIR/nmp-facade.txt is stale; regenerate and commit it"
  }

  "$CATALOG_BIN" active-rows "$ROOT" "$HEAD_REF" > "$TMP/active-rows"
  exec 3< "$TMP/active-rows"
  while IFS= read -r -d '' key <&3; do
    IFS= read -r -d '' _owner <&3
    IFS= read -r -d '' _namespace <&3
    IFS= read -r -d '' _package <&3
    IFS= read -r -d '' _manifest <&3
    IFS= read -r -d '' _library_stem <&3
    IFS= read -r -d '' snapshot <&3
    committed="$TMP/committed-$key"
    git show "$HEAD_REF:$snapshot" > "$committed" 2>/dev/null ||
      fail "head snapshot is unavailable: $snapshot"
    generated="$TMP/generated/${snapshot#docs/surface/}"
    [[ -f "$generated" ]] || fail "regenerator omitted active snapshot: $snapshot"
    cmp -s "$committed" "$generated" || {
      diff -u "$committed" "$generated" >&2 || true
      fail "$snapshot is stale; regenerate and commit it"
    }
  done
  exec 3<&-
fi

log_changed=0
while IFS= read -r -d '' path; do
  [[ $path == "$CHANGE_LOG" ]] && log_changed=1
done < "$TMP/changed-paths"

if [[ $EXPECTED_PROJECTIONS == none && $log_changed == 0 ]]; then
  echo "surface-governance: no governed projection change"
  exit 0
fi
[[ $log_changed == 1 ]] ||
  fail "governed projection changed without an appended change-log entry"

# Exact-prefix history makes every base byte immutable; only appends are
# possible. Both sides come from Git objects, never the working tree.
git show "$BASE_REF:$CHANGE_LOG" > "$TMP/base-log" 2>/dev/null ||
  fail "base change log is unavailable"
git show "$HEAD_REF:$CHANGE_LOG" > "$TMP/head-log" 2>/dev/null ||
  fail "head change log is unavailable"
base_bytes=$(wc -c < "$TMP/base-log" | tr -d ' ')
head_bytes=$(wc -c < "$TMP/head-log" | tr -d ' ')
(( head_bytes > base_bytes )) || fail "change log must grow"
head -c "$base_bytes" "$TMP/head-log" > "$TMP/head-prefix"
cmp -s "$TMP/base-log" "$TMP/head-prefix" ||
  fail "historical change-log content was edited, deleted, or reordered"
tail -c "+$((base_bytes + 1))" "$TMP/head-log" > "$TMP/appended"

entry_count=$(grep -c '^## ' "$TMP/appended" || true)
(( entry_count >= 1 )) || fail "appended log content has no entry heading"
mkdir "$TMP/entries"
awk -v dir="$TMP/entries" '
  /^## / { entry += 1; file = dir "/" entry }
  entry > 0 { print > file }
' "$TMP/appended"
i=1
while (( i <= entry_count )); do
  entry="$TMP/entries/$i"
  grep -Fq "($PR_URL)" "$entry" ||
    fail "appended entry $i must link this exact PR: $PR_URL"
  for field in \
    'Failure evidence' \
    'Changed projections' \
    'Rust / FFI / Swift / Kotlin impact' \
    'Persistence impact' \
    'Diagnostics impact' \
    'Updated falsifiers' \
    'Superseded path removed' \
    'Human signoff'; do
    count=$(grep -c "^- \*\*$field:\*\*" "$entry" || true)
    (( count == 1 )) ||
      fail "appended entry $i needs exactly one '$field' field"
    value=$(grep "^- \*\*$field:\*\*" "$entry" |
      sed 's/^- \*\*[^*]*:\*\*[[:space:]]*//')
    [[ -n ${value//[[:space:]]/} ]] ||
      fail "appended entry $i has an empty '$field' field"
    if [[ $field == "Human signoff" ]]; then
      [[ ! $value =~ [Pp][Ee][Nn][Dd][Ii][Nn][Gg]|[Tt][Bb][Dd]|[Tt][Oo][Dd][Oo]|[Uu][Nn][Kk][Nn][Oo][Ww][Nn] ]] ||
        fail "appended entry $i has a placeholder human signoff"
      [[ $value == *"PR #$PR_NUMBER"* ]] ||
        fail "appended entry $i human signoff must name PR #$PR_NUMBER"
    fi
  done
  entry_projections=$(grep '^- \*\*Changed projections:\*\*' "$entry" |
    sed 's/^- \*\*Changed projections:\*\*[[:space:]]*//')
  [[ $entry_projections == "$EXPECTED_PROJECTIONS" ]] ||
    fail "appended entry $i projections must be exactly: $EXPECTED_PROJECTIONS"
  i=$((i + 1))
done

echo "surface-governance: projections match; component transition and append-only entry are complete"
