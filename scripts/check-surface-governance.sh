#!/usr/bin/env bash
set -euo pipefail

# Three outcomes, three exit codes, so a caller never has to read prose to know
# what happened (#1264):
#
#   0   the head was judged and accepted
#   1   the head was judged and rejected -- a verdict about the proposed change
#   4   the head is not descended from the PR base -- nothing about the head was
#       judged; the branch needs the current base merged in
#   70  the gate never reached a verdict: its inputs, its tools, or the
#       toolchain failed. Not a statement about the proposed change.
#
# Every nonzero code still blocks. The split exists so that "your change is
# illegitimate" and "the gate broke" stop being the same red.
MALFUNCTION_EXIT=70
STALE_BASE_EXIT=4

ROOT=${SURFACE_ROOT:-$(git rev-parse --show-toplevel)}
BASE_REF=${SURFACE_BASE_REF:-}
HEAD_REF=${SURFACE_HEAD_REF:-HEAD}
SNAPSHOT_DIR=${SURFACE_SNAPSHOT_DIR:-docs/surface}
CHANGE_LOG=${SURFACE_CHANGE_LOG:-docs/surface-change-log.md}
REGEN_CMD=${SURFACE_REGEN_CMD:-scripts/regenerate-surface-snapshots.sh}
CATALOG_TOOL_DIR=${SURFACE_CATALOG_TOOL_DIR:-$ROOT/tools/surface-component-catalog}
SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
MIGRATION_CHECK="$SCRIPT_DIR/check-surface-migration-authorization.py"

# A verdict about the proposed head. The wording of these lines is the record
# the change-log, the triage rules, and the issue trail all quote; it does not
# move.
fail() { echo "surface-governance: $*" >&2; exit 1; }
stale_base() { echo "surface-governance: $*" >&2; exit "$STALE_BASE_EXIT"; }
# The gate could not judge anything. A distinct prefix so grep and eyes agree.
malfunction() {
  echo "surface-governance-malfunction: $*" >&2
  exit "$MALFUNCTION_EXIT"
}

# Anything that fails without being routed through one of the three above is by
# definition unplanned, which makes it a malfunction and never a verdict. The
# ERR trap is deliberately not inherited (`set -E` is absent) so a failure
# inside a command substitution is reported once, at the top-level command that
# consumed it.
trap 'malfunction "no verdict was rendered: line $LINENO exited $?"' ERR

cd "$ROOT"
[[ -n "$BASE_REF" ]] || malfunction "SURFACE_BASE_REF is required"
git cat-file -e "$BASE_REF^{commit}" 2>/dev/null ||
  malfunction "base commit is unavailable: $BASE_REF"
git cat-file -e "$HEAD_REF^{commit}" 2>/dev/null ||
  malfunction "head commit is unavailable: $HEAD_REF"

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
[[ -x "$CATALOG_BIN" ]] ||
  malfunction "component catalog tool is unavailable: $CATALOG_BIN"

# The catalog tool judges the head's catalog, and the falsifier suite proves the
# tool itself. A nonzero result here is therefore a verdict about the proposed
# head, not a malfunction. Every classified exit below happens in this shell and
# never inside a command substitution, so the ERR trap cannot recolour it.
projection_set() {
  local value status=0
  value=$("$CATALOG_BIN" projections "$ROOT" "$BASE_REF" "$HEAD_REF") || status=$?
  (( status == 0 )) ||
    fail "component catalog rejected the proposed head: projections (exit $status)"
  if [[ $value != none ]]; then
    printf '%s\n' "$value"
  elif git diff --quiet "$BASE_REF...$HEAD_REF" -- "$CHANGE_LOG"; then
    printf 'none\n'
  else
    printf 'correction\n'
  fi
}

mode=${1:-}
if [[ $mode == "--print-projections" ]]; then
  [[ $# -eq 1 ]] || malfunction "usage: $0 [--print-projections]"
  projection_set
  exit 0
elif [[ $mode == "--print-migration-authorization" ]]; then
  [[ $# -eq 1 ]] ||
    malfunction "usage: $0 [--print-projections|--print-migration-authorization]"
elif [[ $# -ne 0 ]]; then
  malfunction "usage: $0 [--print-projections|--print-migration-authorization]"
fi

PR_NUMBER=${SURFACE_PR_NUMBER:-}
PR_URL=${SURFACE_PR_URL:-}
PASSED_PROJECTIONS=${SURFACE_CHANGED_PROJECTIONS:-}
# These three describe how the gate was invoked, not what the head contains.
[[ $PR_NUMBER =~ ^[1-9][0-9]*$ ]] ||
  malfunction "SURFACE_PR_NUMBER must be the actual numeric PR"
EXPECTED_PR_URL="https://github.com/pablof7z/nmp/pull/$PR_NUMBER"
[[ $PR_URL == "$EXPECTED_PR_URL" ]] ||
  malfunction "SURFACE_PR_URL does not match PR number"
projection_set > "$TMP/expected-projections"
EXPECTED_PROJECTIONS=$(< "$TMP/expected-projections")
[[ $PASSED_PROJECTIONS == "$EXPECTED_PROJECTIONS" ]] ||
  malfunction "changed projection context mismatch: expected $EXPECTED_PROJECTIONS, got ${PASSED_PROJECTIONS:-<empty>}"

transition_status=0
"$CATALOG_BIN" transition "$ROOT" "$BASE_REF" "$HEAD_REF" "$PR_NUMBER" "$PR_URL" \
  > "$TMP/transition-mode" || transition_status=$?
(( transition_status == 0 )) ||
  fail "component catalog rejected the proposed head: transition (exit $transition_status)"
transition_mode=$(< "$TMP/transition-mode")
[[ $transition_mode == steady ]] ||
  malfunction "component catalog returned an unknown transition mode: $transition_mode"

[[ -f "$MIGRATION_CHECK" ]] ||
  malfunction "base-trusted migration verifier is unavailable: $MIGRATION_CHECK"
migration_args=(
  --root "$ROOT"
  --base "$BASE_REF"
  --head "$HEAD_REF"
  --pr-number "$PR_NUMBER"
)
if [[ $mode == "--print-migration-authorization" ]]; then
  [[ ${SURFACE_MIGRATION_ISSUE:-} =~ ^[1-9][0-9]*$ ]] ||
    malfunction "SURFACE_MIGRATION_ISSUE must name the open owning issue"
  print_status=0
  python3 "$MIGRATION_CHECK" "${migration_args[@]}" print-status \
    --issue-number "$SURFACE_MIGRATION_ISSUE" || print_status=$?
  case "$print_status" in
    0) ;;
    "$MALFUNCTION_EXIT")
      malfunction "the migration verifier did not reach a verdict" ;;
    *) fail "the proposed governance migration cannot be authorized" ;;
  esac
  exit 0
fi

# Reusable authorization is a GitHub commit-status record fetched by the
# base-owned workflow. The helper owns both protected-path activation and the
# complete PR/diff/object/issue/status verification. Exit 3 means the PR does
# not touch a protected governance surface.
# `set +e` would not do here: the ERR trap fires on a nonzero status whether or
# not errexit is on, and every one of the verifier's codes below is expected.
migration_status=0
python3 "$MIGRATION_CHECK" "${migration_args[@]}" verify \
  --pr-url "$PR_URL" \
  --pull-request-record "${SURFACE_PR_RECORD:-}" \
  --issue-record "${SURFACE_ISSUE_RECORD:-}" \
  --status-records "${SURFACE_STATUS_RECORDS:-}" >/dev/null || migration_status=$?
# The verifier's own exit codes carry the same three-way split: 1 is a verdict
# on the head, 4 says the head is not on the current base, and 70 says the
# verifier never got far enough to decide.
case "$migration_status" in
  0) ;;
  3) ;;
  # A verdict, and a narrow one: it is about authorization, not about the
  # surface. Everything below this step is skipped, so the line says so rather
  # than leaving a reader to reconstruct it from a convention document.
  1) fail "protected governance migration is not exactly authorized; the change-log and regeneration checks below it did not run" ;;
  "$STALE_BASE_EXIT")
    stale_base "protected migration head is not descended from the current PR base" ;;
  "$MALFUNCTION_EXIT")
    malfunction "the migration verifier did not reach a verdict" ;;
  *)
    malfunction "the migration verifier exited $migration_status without a verdict" ;;
esac

# The ordinary pull_request job runs deterministic regeneration with the same
# protected checker/CI program. The pull_request_target job sets SKIP_REGEN and
# treats the untrusted head strictly as Git data; it never compiles head code.
if [[ ${SURFACE_SKIP_REGEN:-0} != 1 ]]; then
  # A dirty checkout is something the job did to itself; the head cannot cause
  # it, so it is never a verdict.
  [[ -z $(git status --porcelain=v1 --untracked-files=all) ]] ||
    malfunction "deterministic regeneration requires a clean worktree"
  # Regeneration compiles the proposed head. A failure here is overwhelmingly a
  # statement about that head, so it stays a verdict rather than being excused
  # as a gate malfunction.
  regen_status=0
  SURFACE_HEAD_REF="$HEAD_REF" \
    SURFACE_CATALOG_BIN="$CATALOG_BIN" \
    "$REGEN_CMD" --output-dir "$TMP/generated" || regen_status=$?
  (( regen_status == 0 )) ||
    fail "the proposed head could not be regenerated (exit $regen_status)"
  regen_status=0
  SURFACE_HEAD_REF="$HEAD_REF" \
    SURFACE_CATALOG_BIN="$CATALOG_BIN" \
    SURFACE_COMPONENT_ORDER=reverse \
    "$REGEN_CMD" --output-dir "$TMP/generated-reverse" || regen_status=$?
  (( regen_status == 0 )) ||
    fail "the proposed head could not be regenerated in reverse order (exit $regen_status)"
  diff -ru "$TMP/generated" "$TMP/generated-reverse" >/dev/null ||
    fail "catalog-order and reverse-order regeneration differ"

  git show "$HEAD_REF:$SNAPSHOT_DIR/nmp-facade.txt" > "$TMP/committed-facade" 2>/dev/null ||
    fail "head snapshot is unavailable: $SNAPSHOT_DIR/nmp-facade.txt"
  cmp -s "$TMP/committed-facade" "$TMP/generated/nmp-facade.txt" || {
    diff -u "$TMP/committed-facade" "$TMP/generated/nmp-facade.txt" >&2 || true
    fail "$SNAPSHOT_DIR/nmp-facade.txt is stale; regenerate and commit it"
  }

  rows_status=0
  "$CATALOG_BIN" active-rows "$ROOT" "$HEAD_REF" > "$TMP/active-rows" || rows_status=$?
  (( rows_status == 0 )) ||
    fail "component catalog rejected the proposed head: active-rows (exit $rows_status)"
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
git diff --quiet "$BASE_REF...$HEAD_REF" -- "$CHANGE_LOG" || log_changed=1

if [[ $EXPECTED_PROJECTIONS == none && $log_changed == 0 ]]; then
  echo "surface-governance: no governed projection change"
  exit 0
fi
[[ $log_changed == 1 ]] ||
  fail "governed projection changed without an appended change-log entry"

# Exact-prefix history makes every base byte immutable; only appends are
# possible. Both sides come from Git objects, never the working tree.
# The base is the trusted side. If its change log cannot be read the gate has
# lost the record it judges against, which is a malfunction and not a finding
# about the head.
git show "$BASE_REF:$CHANGE_LOG" > "$TMP/base-log" 2>/dev/null ||
  malfunction "base change log is unavailable"
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
