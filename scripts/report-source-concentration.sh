#!/usr/bin/env bash
# #1561: report production/test/documentation/generated line counts per file
# and per workspace package, with review triggers at 500/1,000/1,500
# non-test lines and a flag for packages under ~250 production lines.
#
# This is a REPORT, not a gate: it always exits 0 once it has successfully
# produced output, regardless of how many thresholds a file or package
# crosses. It exits non-zero only for a tool-level failure (missing
# git/cargo/python3, an unreadable file, a broken `cargo metadata` call) --
# see scripts/report-source-concentration.py's module docstring for the
# full classification rules and their limits.
set -euo pipefail

SCRIPT_PATH=${BASH_SOURCE[0]}
SCRIPT_DIR=${SCRIPT_PATH%/*}
[[ $SCRIPT_DIR != "$SCRIPT_PATH" ]] || SCRIPT_DIR=.
source "$SCRIPT_DIR/lib/require-commands.sh" || exit 2
require_commands cargo git python3 || exit 2

ROOT=$(cd "$SCRIPT_DIR/.." && pwd)

python3 "$ROOT/scripts/report-source-concentration.py" "$ROOT" "$@"
