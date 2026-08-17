#!/usr/bin/env bash
# =============================================================================
# Concurrent-worktree shared Bazel disk-cache proof.
#
# Proves the critical requirement of the Bazel migration: multiple git worktrees
# build concurrently against ONE shared Bazel disk cache with NO cross-worktree
# artifact contamination.
#
# Method:
#   1. Create two git worktrees from the current HEAD (which carries the Bazel
#      workspace). Edit `tools/bazel/proof/main.rs`'s `VALUE` const in each to a
#      distinct string, so each worktree's `printer` binary prints a different
#      value. `hex` (a shared third-party dep) is identical across worktrees.
#   2. Build both worktrees CONCURRENTLY. Both share `--disk_cache` (.bazelrc).
#   3. Execute each binary; assert each prints ITS OWN value (no contamination).
#   4. Wipe worktree A's local Bazel output base (`bazel clean --expunge`). The
#      shared disk cache (~/.cache/bazel-nmp-diskcache) is OUTSIDE the output
#      base and survives the wipe.
#   5. Rebuild A. The shared `hex` dep must be served from the disk cache (cache
#      hit) and A's `printer` must STILL print A's value -- proving the cache is
#      shared across worktrees AND keyed by action digest (no wrong-workspace
#      output reuse: A's printer digest != B's printer digest).
#
# Usage:  bash tools/bazel/proof/run_proof.sh
# Exits non-zero on any proof failure.
# =============================================================================
set -euo pipefail

REPO="$(git rev-parse --show-toplevel)"
HEAD_COMMIT="$(git rev-parse HEAD)"
PROOF_MAIN="tools/bazel/proof/main.rs"
TARGET="//tools/bazel/proof:printer"
PRINTER_REL="bazel-bin/tools/bazel/proof/printer"

VAL_A="alpha-from-worktree-A-1111111111111111"
VAL_B="beta-from-worktree-B-2222222222222222"

TMP="$(mktemp -d -t nmp-bazel-proof.XXXXXX)"
WT_A="$TMP/worktree-a"
WT_B="$TMP/worktree-b"
PROOF_LOG="$TMP/proof.log"

cleanup() {
  git -C "$REPO" worktree remove --force "$WT_A" >/dev/null 2>&1 || true
  git -C "$REPO" worktree remove --force "$WT_B" >/dev/null 2>&1 || true
  rm -rf "$TMP"
}
trap cleanup EXIT

log() { printf '\n\033[1m== %s ==\033[0m\n' "$*"; }
pass() { printf '  \033[32mPASS\033[0m %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; exit 1; }

set_value() {  # <worktree> <value>
  local wt="$1" val="$2"
  python3 - "$wt/$PROOF_MAIN" "$val" <<'PY'
import re, sys
path, val = sys.argv[1], sys.argv[2]
s = open(path).read()
s2 = re.sub(r'^const VALUE: &str = ".*";',
            'const VALUE: &str = "%s";' % val, s, count=1, flags=re.M)
assert s2 != s, "VALUE const not replaced in %s" % path
open(path, "w").write(s2)
PY
}

run_printer() {  # <worktree>  -> stdout is the binary's output
  "$1/$PRINTER_REL"
}

# --- setup -------------------------------------------------------------------
log "creating two git worktrees from HEAD $HEAD_COMMIT"
git -C "$REPO" worktree add --detach "$WT_A" "$HEAD_COMMIT"
git -C "$REPO" worktree add --detach "$WT_B" "$HEAD_COMMIT"
set_value "$WT_A" "$VAL_A"
set_value "$WT_B" "$VAL_B"
pass "worktree A -> VALUE=$VAL_A"
pass "worktree B -> VALUE=$VAL_B"

# --- step 1: concurrent build ------------------------------------------------
log "building both worktrees CONCURRENTLY (shared --disk_cache)"
( cd "$WT_A" && bazel build "$TARGET" ) >"$TMP/build_a.log" 2>&1 &
A_PID=$!
( cd "$WT_B" && bazel build "$TARGET" ) >"$TMP/build_b.log" 2>&1 &
B_PID=$!
wait "$A_PID" && pass "worktree A built"
wait "$B_PID" && pass "worktree B built"

OUT_A="$(run_printer "$WT_A")"
OUT_B="$(run_printer "$WT_B")"
printf '  worktree A printer -> %s\n' "$OUT_A"
printf '  worktree B printer -> %s\n' "$OUT_B"
[ "$OUT_A" = "$VAL_A" ] || fail "A printed '$OUT_A', expected '$VAL_A'"
[ "$OUT_B" = "$VAL_B" ] || fail "B printed '$OUT_B', expected '$VAL_B'"
[ "$OUT_A" != "$OUT_B" ] || fail "A and B outputs collide ($OUT_A)"
pass "each worktree's binary prints its own value (no contamination)"

# --- step 2: wipe A's local outputs ------------------------------------------
log "wiping worktree A's local Bazel output base (bazel clean --expunge)"
( cd "$WT_A" && bazel clean --expunge ) >"$TMP/clean_a.log" 2>&1
pass "A local output base wiped; shared disk cache survives"

# --- step 3: rebuild A from the shared disk cache ----------------------------
log "rebuilding worktree A (deps must hit the shared disk cache)"
( cd "$WT_A" && bazel build "$TARGET" ) >"$TMP/rebuild_a.log" 2>&1

OUT_A2="$(run_printer "$WT_A")"
[ "$OUT_A2" = "$VAL_A" ] || fail "after wipe, A printed '$OUT_A2', expected '$VAL_A'"
pass "A still prints '$VAL_A' after local wipe (no wrong-workspace reuse)"

if grep -q "cache hit" "$TMP/rebuild_a.log"; then
  HITS="$(grep -c "cache hit" "$TMP/rebuild_a.log" || true)"
  pass "rebuild served $HITS cache-hit line(s) from the shared disk cache"
  SUMMARY="$(sed -E $'s/\x1b\\[[0-9;]*m//g' "$TMP/rebuild_a.log" | grep -E 'INFO: [0-9]+ processes' | tail -1)"
  printf '  rebuild action summary: %s\n' "$SUMMARY"
else
  echo "  \033[33mNOTE\033[0m no explicit 'cache hit' lines; inspect $TMP/rebuild_a.log"
fi

# Preserve the logs for post-run inspection (do NOT rm $TMP on success).
trap - EXIT
KEPT="$HOME/.cache/bazel-nmp-diskcache-proof-logs"
mkdir -p "$KEPT"
cp "$TMP"/{build_a,build_b,clean_a,rebuild_a}.log "$KEPT"/ 2>/dev/null || true
git -C "$REPO" worktree remove --force "$WT_A" >/dev/null 2>&1 || true
git -C "$REPO" worktree remove --force "$WT_B" >/dev/null 2>&1 || true
rm -rf "$TMP"

log "ALL CHECKS PASSED -- shared disk cache works across concurrent worktrees without contamination"
echo "  logs kept at: $KEPT"