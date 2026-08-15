#!/usr/bin/env bash
#
# Static census gate for #1624: the detached-materializer plugin shape is gone.
#
# Semantic-write capability code is now trusted, compiled, construction-time
# input that runs directly on the engine thread (read snapshot, close the Redb
# transaction, transform, validate, commit). This script proves the old
# plugin lifecycle is absent from production source: no after-start
# registration, no per-call OS-thread spawn, no panic translation, no
# completion command/id/pending maps, and no blocked-callback liveness test.
#
# The behavioral thread-census test runs in an isolated child process and
# drives both initial and source-successor materialization. This census is its
# structural companion: restoring any deleted lifecycle spelling fails here
# even before behavior is exercised.
#
# Usage:
#   scripts/check-no-detached-materializer.sh
#
# Exit status:
#   0 the detached-materializer shape is absent
#   1 a deleted production path was reintroduced
set -euo pipefail

SCRIPT_PATH=${BASH_SOURCE[0]}
SCRIPT_DIR=${SCRIPT_PATH%/*}
[[ $SCRIPT_DIR != "$SCRIPT_PATH" ]] || SCRIPT_DIR=.
ROOT="$SCRIPT_DIR/.."

# Fail closed (#1007): declare the external commands this checker needs so an
# environment missing them reports the prerequisite refusal instead of a
# usage branch or a false pass.
# shellcheck source=scripts/lib/require-commands.sh
. "$ROOT/scripts/lib/require-commands.sh"
require_commands grep rm

# Crate source roots. Inline test modules are scanned too; none should preserve
# the deleted plugin vocabulary as a second test-only architecture.
PRODUCTION=(
    "$ROOT/crates/nmp/src"
    "$ROOT/crates/nmp-ffi/src"
    "$ROOT/crates/nmp-nip02/src"
    "$ROOT/crates/nmp-nip29/src"
)

# The files that carry capability entry: where the trait lives, where the call
# is built and run, and the two production modules that call
# `run_replaceable_materialization`. `catch_unwind` is scanned only here, not
# across the whole crate, because `crates/nmp/src/runtime/auth.rs` owns an
# unrelated and legitimate use for a tokio task adapter. Scoping it keeps the
# check exact instead of forcing a blanket allowance that would also stop it
# catching the real thing.
MATERIALIZER_PATH=(
    "$ROOT/crates/nmp/src/replaceable_materializer.rs"
    "$ROOT/crates/nmp/src/core/write/replaceable_operation.rs"
    "$ROOT/crates/nmp/src/core/write.rs"
    "$ROOT/crates/nmp/src/runtime/mod.rs"
)

MATERIALIZER_PATH_PATTERNS=(
    # Panic containment/translation for trusted, compiled capability code. A
    # panic there is an ordinary NMP bug to fix, not something NMP contains.
    'catch_unwind'
)

# Each pattern names a piece of the deleted plugin lifecycle. Restoring any
# one is a regression of the #1624 contraction.
PATTERNS=(
    # After-start registration / replacement lifecycle.
    'add_replaceable_materializer'
    'ReplaceableMaterializationCompleted'
    'StartReplaceableSuccessor'
    'CompleteReplaceableSuccessor'
    'PreparedReplaceableSuccessor'
    'ReplaceableSuccessorContinuation'
    'MaterializeReplaceableSuccessor'
    'complete_replaceable_successor_materialization'
    'semantic_successor_requests'
    # Per-call OS-thread spawn path and its thread name.
    'spawn_replaceable_materialization'
    'nmp-replaceable-materializer'
    # Panic translation / containment for trusted capability code.
    'ReplaceableMaterializationOutcome::Panicked'
    'ReplaceableMaterializationOutcome::ThreadUnavailable'
    # Blocked-callback liveness tests. #1624 withdraws every responsiveness
    # promise about capability code, so the tests that asserted one must not
    # come back under any of their old spellings.
    'blocked_[a-z_]*materializer'
    'materializer_is_blocked'
)

status=0
scan() {
    local pattern=$1
    shift
    if grep -REIn --exclude-dir=target --exclude='*.rs.bk' -e "$pattern" "$@" \
        >/tmp/nmp-detached-materializer-hits.$$ 2>/dev/null; then
        echo "error: reintroduced detached-materializer path: '$pattern'" >&2
        cat /tmp/nmp-detached-materializer-hits.$$ >&2
        status=1
    fi
    rm -f /tmp/nmp-detached-materializer-hits.$$
}

for pattern in "${PATTERNS[@]}"; do
    scan "$pattern" "${PRODUCTION[@]}"
done

for pattern in "${MATERIALIZER_PATH_PATTERNS[@]}"; do
    scan "$pattern" "${MATERIALIZER_PATH[@]}"
done

if [[ $status -eq 0 ]]; then
    echo "OK: no production detached-materializer path found."
fi
exit $status
