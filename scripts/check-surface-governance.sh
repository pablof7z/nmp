#!/usr/bin/env bash
set -euo pipefail

ROOT=${SURFACE_ROOT:-$(git rev-parse --show-toplevel)}
BASE_REF=${SURFACE_BASE_REF:-}
HEAD_REF=${SURFACE_HEAD_REF:-HEAD}
SNAPSHOT_DIR=${SURFACE_SNAPSHOT_DIR:-docs/surface}
CHANGE_LOG=${SURFACE_CHANGE_LOG:-docs/surface-change-log.md}
REGEN_CMD=${SURFACE_REGEN_CMD:-scripts/regenerate-surface-snapshots.sh}

fail() { echo "surface-governance: $*" >&2; exit 1; }

cd "$ROOT"
[[ -n "$BASE_REF" ]] || fail "SURFACE_BASE_REF is required"
git cat-file -e "$BASE_REF^{commit}" 2>/dev/null || fail "base commit is unavailable: $BASE_REF"
git cat-file -e "$HEAD_REF^{commit}" 2>/dev/null || fail "head commit is unavailable: $HEAD_REF"

changed_paths=$(git diff --name-only "$BASE_REF...$HEAD_REF")

projection_set() {
  local rust=0 ffi=0 swift=0 kotlin=0 log=0 path result=""
  while IFS= read -r path; do
    case "$path" in
      "$SNAPSHOT_DIR/nmp-facade.txt") rust=1 ;;
      "$SNAPSHOT_DIR/nmp-ffi-component.txt") ffi=1 ;;
      Packages/NMP/Sources/NMP/*|Packages/NMP/Package.swift) swift=1 ;;
      Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/*|\
      Packages/NMPKotlin/build.gradle.kts|Packages/NMPKotlin/settings.gradle.kts) kotlin=1 ;;
      "$CHANGE_LOG") log=1 ;;
    esac
  done <<< "$changed_paths"
  for pair in "ffi:$ffi" "kotlin:$kotlin" "rust:$rust" "swift:$swift"; do
    if [[ ${pair#*:} == 1 ]]; then
      result=${result:+$result,}${pair%%:*}
    fi
  done
  if [[ -n "$result" ]]; then
    printf '%s\n' "$result"
  elif [[ $log == 1 ]]; then
    printf 'correction\n'
  else
    printf 'none\n'
  fi
}

if [[ ${1:-} == "--print-projections" ]]; then
  [[ $# -eq 1 ]] || fail "usage: $0 [--print-projections]"
  projection_set
  exit 0
elif [[ $# -ne 0 ]]; then
  fail "usage: $0 [--print-projections]"
fi

# The pull_request_target workflow and these scripts are trusted from the base
# revision. A PR cannot replace the program that judges itself. Governance
# changes require an explicit owner-controlled bootstrap/update outside this
# ordinary PR path, after which the new base protects subsequent PRs.
bootstrap=0
if ! git cat-file -e "$BASE_REF:.github/workflows/surface-governance.yml" 2>/dev/null &&
   ! git cat-file -e "$BASE_REF:scripts/check-surface-governance.sh" 2>/dev/null; then
  bootstrap=1
fi
while IFS= read -r path; do
  case "$path" in
    .github/workflows/ci.yml|\
    .github/workflows/surface-governance.yml|\
    scripts/check-surface-governance.sh|\
    scripts/install-surface-tools.sh|\
    scripts/regenerate-surface-snapshots.sh|\
    scripts/test-install-surface-tools.sh|\
    scripts/test-surface-governance.sh|\
    tools/component-interface-snapshot/Cargo.lock|\
    tools/component-interface-snapshot/Cargo.toml|\
    tools/component-interface-snapshot/src/main.rs|\
    tools/surface-toolchain.env)
      if [[ ${SURFACE_BOOTSTRAP:-0} == 1 && $bootstrap == 1 ]]; then
        continue
      fi
      fail "protected governance program changed: $path"
      ;;
  esac
done <<< "$changed_paths"

PR_NUMBER=${SURFACE_PR_NUMBER:-}
PR_URL=${SURFACE_PR_URL:-}
PASSED_PROJECTIONS=${SURFACE_CHANGED_PROJECTIONS:-}
[[ $PR_NUMBER =~ ^[1-9][0-9]*$ ]] || fail "SURFACE_PR_NUMBER must be the actual numeric PR"
EXPECTED_PR_URL="https://github.com/pablof7z/nmp/pull/$PR_NUMBER"
[[ $PR_URL == "$EXPECTED_PR_URL" ]] || fail "SURFACE_PR_URL does not match PR number"
EXPECTED_PROJECTIONS=$(projection_set)
[[ $PASSED_PROJECTIONS == "$EXPECTED_PROJECTIONS" ]] ||
  fail "changed projection context mismatch: expected $EXPECTED_PROJECTIONS, got ${PASSED_PROJECTIONS:-<empty>}"

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

# The ordinary pull_request job runs deterministic regeneration with the same
# protected checker/CI program. The pull_request_target job sets SKIP_REGEN and
# treats the untrusted head strictly as git data; it never compiles head code.
if [[ ${SURFACE_SKIP_REGEN:-0} != 1 ]]; then
  "$REGEN_CMD" --output-dir "$TMP/generated"
  for name in nmp-facade.txt nmp-ffi-component.txt; do
    git show "$HEAD_REF:$SNAPSHOT_DIR/$name" > "$TMP/committed-$name" 2>/dev/null ||
      fail "head snapshot is unavailable: $SNAPSHOT_DIR/$name"
    cmp -s "$TMP/committed-$name" "$TMP/generated/$name" || {
      diff -u "$TMP/committed-$name" "$TMP/generated/$name" >&2 || true
      fail "$SNAPSHOT_DIR/$name is stale; regenerate and commit it"
    }
  done
fi

# The introductory PR has no trusted base program and intentionally seeds only
# historical entries (#67/#73/#77), not a fabricated self-entry. Its ordinary
# CI proves deterministic regeneration; existing CI/manual review owns the
# bootstrap diff. This branch is unreachable once either trusted base file
# exists, regardless of a head-provided flag.
if [[ ${SURFACE_BOOTSTRAP:-0} == 1 && $bootstrap == 1 ]]; then
  echo "surface-governance: bootstrap regeneration matches; policy activates after merge"
  exit 0
fi

log_changed=0
relevant_changed=0
while IFS= read -r path; do
  if [[ $path == "$CHANGE_LOG" ]]; then
    log_changed=1
  fi
  case "$path" in
    "$SNAPSHOT_DIR/nmp-facade.txt"|"$SNAPSHOT_DIR/nmp-ffi-component.txt"|\
    Packages/NMP/Sources/NMP/*|Packages/NMP/Package.swift|\
    Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/*|\
    Packages/NMPKotlin/build.gradle.kts|Packages/NMPKotlin/settings.gradle.kts)
      relevant_changed=1
      ;;
  esac
done <<< "$changed_paths"

if [[ $relevant_changed -eq 0 && $log_changed -eq 0 ]]; then
  echo "surface-governance: no governed projection change"
  exit 0
fi
[[ $log_changed -eq 1 ]] || fail "governed projection changed without an appended change-log entry"

# Unit F bootstraps this file. Once present on the base branch, the exact-prefix
# invariant makes every historical byte immutable; only appends are possible.
if ! git show "$BASE_REF:$CHANGE_LOG" > "$TMP/base-log" 2>/dev/null; then
  : > "$TMP/base-log"
fi
git show "$HEAD_REF:$CHANGE_LOG" > "$TMP/head-log" 2>/dev/null || fail "head change log is unavailable"
base_bytes=$(wc -c < "$TMP/base-log" | tr -d ' ')
head_bytes=$(wc -c < "$TMP/head-log" | tr -d ' ')
(( head_bytes > base_bytes )) || fail "change log must grow"
if (( base_bytes == 0 )); then
  : > "$TMP/head-prefix"
else
  head -c "$base_bytes" "$TMP/head-log" > "$TMP/head-prefix"
fi
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
  grep -Fq "($PR_URL)" "$entry" || fail "appended entry $i must link this exact PR: $PR_URL"
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
    (( count == 1 )) || fail "appended entry $i needs exactly one '$field' field"
    value=$(grep "^- \*\*$field:\*\*" "$entry" | sed 's/^- \*\*[^*]*:\*\*[[:space:]]*//')
    [[ -n ${value//[[:space:]]/} ]] || fail "appended entry $i has an empty '$field' field"
    if [[ $field == "Human signoff" ]]; then
      [[ ! $value =~ [Pp][Ee][Nn][Dd][Ii][Nn][Gg]|[Tt][Bb][Dd]|[Tt][Oo][Dd][Oo]|[Uu][Nn][Kk][Nn][Oo][Ww][Nn] ]] ||
        fail "appended entry $i has a placeholder human signoff"
      [[ $value == *"PR #$PR_NUMBER"* ]] ||
        fail "appended entry $i human signoff must name PR #$PR_NUMBER"
    fi
  done
  projections=$(grep '^- \*\*Changed projections:\*\*' "$entry" |
    sed 's/^- \*\*Changed projections:\*\*[[:space:]]*//')
  [[ $projections == "$EXPECTED_PROJECTIONS" ]] ||
    fail "appended entry $i projections must be exactly: $EXPECTED_PROJECTIONS"
  i=$((i + 1))
done

echo "surface-governance: projections match; append-only entry is complete"
