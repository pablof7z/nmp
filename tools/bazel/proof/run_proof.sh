#!/usr/bin/env bash
# =============================================================================
# Concurrent-worktree shared Bazel disk-cache proof.
#
# The requirement: several git worktrees build at once against ONE shared
# Bazel disk cache, reusing each other's compiled artifacts, and never serving
# one worktree an artifact built from another worktree's sources.
#
# The cache key is the action digest, which does not contain the workspace
# path. That is what makes sharing possible and what makes contamination
# conceivable, so it is the thing under test.
#
# Four rounds, each answering a question the previous one leaves open:
#
#   1. CONCURRENT COLD    N worktrees build at the same time, each with a
#                         distinct `VALUE`, plus one large identical real
#                         crate. Distinct outputs must stay distinct; the
#                         identical crate must be shared.
#   2. INCREMENTAL EDIT   Change every worktree's VALUE again and rebuild.
#                         Proves invalidation is per-worktree: a stale hit
#                         here would print round 1's value.
#   3. ALL WIPED          `bazel clean --expunge` in EVERY worktree, so no
#                         local output base survives anywhere, then rebuild
#                         concurrently. Everything must come back from the
#                         shared cache alone, still with the right values.
#   4. CONCURRENT TEST    `bazel test` of a real target in every worktree at
#                         once, to put the cache under simultaneous
#                         read/write load rather than serial access.
#
# What round 3 adds over "wipe one worktree": with a single worktree wiped the
# survivor's output base can still explain a correct result. With all of them
# wiped the shared cache is the only remaining source.
#
# Usage:  bash tools/bazel/proof/run_proof.sh [N]      (default N=4)
# Exits non-zero on any proof failure.
# =============================================================================
set -euo pipefail

N="${1:-4}"
REPO="$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel)"
HEAD_COMMIT="$(git -C "$REPO" rev-parse HEAD)"
PROOF_MAIN="tools/bazel/proof/main.rs"
PRINTER="//tools/bazel/proof:printer"
PRINTER_REL="bazel-bin/tools/bazel/proof/printer"
# A real, chunky first-party crate, identical in every worktree: this is what
# the shared cache is actually for. The printer alone would prove sharing of
# one third-party crate and nothing about the workspace.
SHARED_TARGET="//crates/nmp-store:lib"
# A real test, run concurrently in round 4.
TEST_TARGET="//crates/nmp-grammar:unit_tests"

TMP="$(mktemp -d -t nmp-bazel-proof.XXXXXX)"
WT=()
for i in $(seq 1 "$N"); do WT+=("$TMP/worktree-$i"); done

cleanup() {
  for w in "${WT[@]}"; do
    # Shut the server down first: an orphaned Bazel JVM holds its output base
    # (hundreds of MB) open after the worktree is gone.
    ( cd "$w" 2>/dev/null && bazel shutdown ) >/dev/null 2>&1 || true
    git -C "$REPO" worktree remove --force "$w" >/dev/null 2>&1 || true
  done
  rm -rf "$TMP"
  git -C "$REPO" worktree prune >/dev/null 2>&1 || true
}
trap cleanup EXIT

log()  { printf '\n\033[1m== %s ==\033[0m\n' "$*"; }
pass() { printf '  \033[32mPASS\033[0m %s\n' "$*"; }
fail() { printf '  \033[31mFAIL\033[0m %s\n' "$*"; exit 1; }

value_for() { printf 'worktree-%s-round-%s-%s' "$1" "$2" "$(printf '%.0s7' $(seq 1 8))"; }

set_value() {  # <worktree-dir> <value>
  python3 - "$1/$PROOF_MAIN" "$2" <<'PY'
import re, sys
path, val = sys.argv[1], sys.argv[2]
s = open(path).read()
s2 = re.sub(r'^const VALUE: &str = ".*";',
            'const VALUE: &str = "%s";' % val, s, count=1, flags=re.M)
assert s2 != s, "VALUE const not replaced in %s" % path
open(path, "w").write(s2)
PY
}

# Build every worktree AT THE SAME TIME and wait for all of them. Returns
# non-zero if any build failed.
build_all_concurrently() {  # <round-label> <targets...>
  local round="$1"; shift
  local pids=() rc=0
  for i in $(seq 1 "$N"); do
    ( cd "${WT[$((i-1))]}" && bazel build "$@" ) >"$TMP/build-$round-$i.log" 2>&1 &
    pids+=($!)
  done
  for i in $(seq 1 "$N"); do
    wait "${pids[$((i-1))]}" || { echo "  worktree $i build failed; see $TMP/build-$round-$i.log"; rc=1; }
  done
  return $rc
}

# The contamination oracle: worktree i's binary must print worktree i's value
# for THIS round, and no two worktrees may print the same thing.
# <value-round> is which round's edit the binaries should still reflect --
# not necessarily the round being run, since round 3 rebuilds without editing.
assert_values() {  # <value-round> <label>
  local vround="$1" label="$2" seen="" out expected
  for i in $(seq 1 "$N"); do
    expected="$(value_for "$i" "$vround")"
    out="$("${WT[$((i-1))]}/$PRINTER_REL")"
    [ "$out" = "$expected" ] || fail "$label: worktree $i printed '$out', expected '$expected'"
    case "$seen" in *"[$out]"*) fail "$label: worktree $i's output collides with an earlier worktree";; esac
    seen="$seen[$out]"
  done
  pass "$label: all $N worktrees printed their own distinct value"
}

# Bazel reports disk-cache reuse in the process summary line.
report_cache() {  # <round>
  for i in $(seq 1 "$N"); do
    local line
    line="$(sed -E $'s/\x1b\\[[0-9;]*m//g' "$TMP/build-$1-$i.log" | grep -E '^INFO: [0-9]+ process' | tail -1 || true)"
    printf '    worktree %s: %s\n' "$i" "${line:-<no process summary>}"
  done
}

# --- setup -------------------------------------------------------------------
log "creating $N git worktrees from HEAD $HEAD_COMMIT"
for i in $(seq 1 "$N"); do
  git -C "$REPO" worktree add --detach "${WT[$((i-1))]}" "$HEAD_COMMIT" >/dev/null
  set_value "${WT[$((i-1))]}" "$(value_for "$i" 1)"
done
pass "$N worktrees created, each with a distinct VALUE"

# --- round 1: concurrent cold build -----------------------------------------
log "round 1/4: $N worktrees building CONCURRENTLY (shared --disk_cache)"
build_all_concurrently 1 "$PRINTER" "$SHARED_TARGET" || fail "round 1: a concurrent build failed"
pass "all $N concurrent builds succeeded"
assert_values 1 "round 1"
report_cache 1

# --- round 2: incremental edit ----------------------------------------------
# A stale or cross-worktree cache hit shows up here as round 1's value.
log "round 2/4: incremental edit in every worktree, concurrent rebuild"
for i in $(seq 1 "$N"); do set_value "${WT[$((i-1))]}" "$(value_for "$i" 2)"; done
build_all_concurrently 2 "$PRINTER" "$SHARED_TARGET" || fail "round 2: a concurrent rebuild failed"
assert_values 2 "round 2"
pass "round 2: every worktree invalidated its own output and no other's"
report_cache 2

# --- round 3: every worktree wiped ------------------------------------------
# With no local output base left anywhere, the shared disk cache is the only
# thing that can serve the identical actions.
log "round 3/4: bazel clean --expunge in ALL $N worktrees, then concurrent rebuild"
for i in $(seq 1 "$N"); do
  ( cd "${WT[$((i-1))]}" && bazel clean --expunge ) >"$TMP/clean-$i.log" 2>&1
done
pass "all $N local output bases wiped (shared disk cache is outside them and survives)"
build_all_concurrently 3 "$PRINTER" "$SHARED_TARGET" || fail "round 3: rebuild from cold local state failed"
# No edit in this round, so the binaries must still carry round 2's values --
# now reproduced from the shared cache alone.
assert_values 2 "round 3"
pass "round 3: after wiping every output base, all $N still print their own value"
report_cache 3

# --- round 4: concurrent test under load ------------------------------------
log "round 4/4: concurrent 'bazel test $TEST_TARGET' in all $N worktrees"
pids=(); rc=0
for i in $(seq 1 "$N"); do
  ( cd "${WT[$((i-1))]}" && bazel test "$TEST_TARGET" --nocache_test_results ) >"$TMP/test-$i.log" 2>&1 &
  pids+=($!)
done
for i in $(seq 1 "$N"); do
  wait "${pids[$((i-1))]}" || { echo "  worktree $i test failed; see $TMP/test-$i.log"; rc=1; }
done
[ "$rc" -eq 0 ] || fail "round 4: concurrent tests failed under shared-cache contention"
pass "round 4: $N concurrent test runs against one disk cache all passed"

log "PROOF COMPLETE"
printf '  %s worktrees, 4 rounds, no contamination observed.\n' "$N"
printf '  disk cache: %s\n' "$(du -sh ~/.cache/bazel-nmp-diskcache 2>/dev/null | cut -f1)"
