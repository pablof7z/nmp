#!/usr/bin/env bash
# #1310: falsifies skills/nmp/scripts/validate_skill.py's own rejection
# behaviour (skills/nmp/tests/test_validate_skill.py), so a regression in
# the checker that check-skill-currency.sh relies on fails the build instead
# of silently validating nothing. Before this issue, nothing ran these
# tests either.
#
# One test only runs where the workstation-only official skill-creator
# validator is installed and is skipped, not failed, everywhere else
# (including CI, which has no such tool by design -- see
# check-skill-currency.sh).
set -euo pipefail

SCRIPT_PATH=${BASH_SOURCE[0]}
SCRIPT_DIR=${SCRIPT_PATH%/*}
[[ $SCRIPT_DIR != "$SCRIPT_PATH" ]] || SCRIPT_DIR=.
source "$SCRIPT_DIR/lib/require-commands.sh" || exit 2
require_commands git python3 || exit 2

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

python3 -m unittest discover -s skills/nmp/tests -p 'test_*.py' -v
