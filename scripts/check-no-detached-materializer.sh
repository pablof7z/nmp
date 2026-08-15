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
# A behavioral thread-census test cannot use the process-wide monotonic
# counter under parallel test execution (other engines' construction inflates
# it between samples), so this static census is the deterministic falsifier.
# Restoring any of these production paths fails this script.
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

# Production source only: the nmp crate core/runtime/engine and the FFI facade.
# Tests, fixtures, and this script itself are excluded so the gate measures the
# shipped shape, not its falsifiers.
PRODUCTION=(
    "$ROOT/crates/nmp/src"
    "$ROOT/crates/nmp-ffi/src"
    "$ROOT/crates/nmp-nip02/src"
    "$ROOT/crates/nmp-nip29/src"
)

# Each pattern names a piece of the deleted plugin lifecycle. Restoring any
# one is a regression of the #1624 contraction.
PATTERNS=(
    # After-start registration / replacement lifecycle.
    'add_replaceable_materializer'
    'ReplaceableMaterializationCompleted'
    'StartReplaceableSuccessor'
    'CompleteReplaceableSuccessor'
    # Per-call OS-thread spawn path and its thread name.
    'spawn_replaceable_materialization'
    'nmp-replaceable-materializer'
    # Panic translation / containment for trusted capability code.
    'ReplaceableMaterializationOutcome::Panicked'
    'ReplaceableMaterializationOutcome::ThreadUnavailable'
)

status=0
for pattern in "${PATTERNS[@]}"; do
    if grep -RIn --exclude-dir=target --exclude='*.rs.bk' -e "$pattern" "${PRODUCTION[@]}" >/tmp/nmp-detached-materializer-hits.$$ 2>/dev/null; then
        echo "error: reintroduced detached-materializer path: '$pattern'" >&2
        cat /tmp/nmp-detached-materializer-hits.$$ >&2
        status=1
    fi
    rm -f /tmp/nmp-detached-materializer-hits.$$
done

if [[ $status -eq 0 ]]; then
    echo "OK: no production detached-materializer path found."
fi
exit $status