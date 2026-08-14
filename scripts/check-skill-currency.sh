#!/usr/bin/env bash
# #1310: skills/nmp/SKILL.md's Verified-Revision pin claims that the skill's
# declared sources (skills/nmp/references/source-map.md) were audited as of
# a specific commit. Nothing enforced that claim: #1177 found the pin 332
# commits stale, on a commit that was not even an ancestor of master,
# because no CI job ran skills/nmp/scripts/validate_skill.py. This script is
# that job's mechanism.
#
# The bundled validator also runs the *official* skill-creator packaging
# check (agents/openai.yaml shape, forbidden files, etc.) through a
# workstation/agent-local tool with no CI equivalent
# (~/.codex/skills/.system/skill-creator). #1310 keeps that half a separate,
# workstation-only step by design, so this script runs the validator with
# --skip-official and gates only on the currency check: the pin's existence,
# its ancestry, and whether any declared source has drifted since it.
set -euo pipefail

SCRIPT_PATH=${BASH_SOURCE[0]}
SCRIPT_DIR=${SCRIPT_PATH%/*}
[[ $SCRIPT_DIR != "$SCRIPT_PATH" ]] || SCRIPT_DIR=.
source "$SCRIPT_DIR/lib/require-commands.sh" || exit 2
require_commands git python3 || exit 2

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

python3 skills/nmp/scripts/validate_skill.py skills/nmp --repo-root "$ROOT" --skip-official
