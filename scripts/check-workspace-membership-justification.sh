#!/usr/bin/env bash
# #1559: the dependency-direction policy (#922) answers whether a package may
# REACH another package. Nothing answered whether a package DESERVES TO
# EXIST -- that is how `nmp-executor` survived the deletion of the executor
# itself, and it is why #806, #1368, and #13 currently propose crates for
# reasons a module can satisfy just as well.
#
# This is a script plus a text record, not a package taxonomy: every
# `[workspace] members` entry in Cargo.toml must have a matching entry in
# workspace-membership-justifications.json. An entry with no record fails
# closed. Every member as of #1559 is recorded as `grandfathered: true` --
# retroactively litigating 30 existing crates is not this gate's job, and a
# grandfathered entry carries no other field. A newly added member must
# instead record, in a form this gate can read:
#   - module_insufficient_because: why a MODULE cannot do this
#   - isolates_dependencies:       the unique normal dependencies it isolates
#   - owns_artifact_or_lifecycle:  the independent artifact or lifecycle it owns
#   - expected_consumers:          its expected direct consumers (non-empty)
#   - breaks_cycle:                the dependency cycle it breaks, only when
#                                   that is the actual justification
#
# "No other package owns this NIP" is refused explicitly, mechanically, as
# `module_insufficient_because` text: a Rust module can be the sole semantic
# owner of a NIP, an event kind, or a pipeline stage, so unclaimed package
# ownership alone never shows a module is insufficient. This is deliberately
# the one form of that non-argument the gate can catch by text, the same
# narrow posture as scripts/check-no-compatibility-surface.sh.
#
# A record entry with no matching workspace member is refused too: a
# justification for a crate that already left the workspace is exactly the
# kind of stale compatibility surface #1559's own repository forbids.
set -euo pipefail

SCRIPT_PATH=${BASH_SOURCE[0]}
SCRIPT_DIR=${SCRIPT_PATH%/*}
[[ $SCRIPT_DIR != "$SCRIPT_PATH" ]] || SCRIPT_DIR=.
# shellcheck source=scripts/lib/require-commands.sh
source "$SCRIPT_DIR/lib/require-commands.sh" || exit 2
require_commands python3 || exit 2

ROOT=$(cd "$SCRIPT_DIR/.." && pwd)

if [[ $# -gt 2 ]]; then
  echo "workspace-membership-justification: usage: $0 [Cargo.toml] [justifications.json]" >&2
  exit 2
fi

MANIFEST=${1:-"$ROOT/Cargo.toml"}
RECORD=${2:-"$ROOT/scripts/workspace-membership-justifications.json"}

[[ -f $MANIFEST ]] || {
  echo "workspace-membership-justification: manifest not found: $MANIFEST" >&2
  exit 2
}
[[ -f $RECORD ]] || {
  echo "workspace-membership-justification: justification record not found: $RECORD" >&2
  exit 2
}

python3 - "$MANIFEST" "$RECORD" <<'PY'
import json
import re
import sys
import tomllib

manifest_path, record_path = sys.argv[1], sys.argv[2]


def fail(message):
    print(f"workspace-membership-justification: {message}", file=sys.stderr)


try:
    with open(manifest_path, "rb") as handle:
        manifest = tomllib.load(handle)
except (OSError, tomllib.TOMLDecodeError) as error:
    fail(f"cannot read manifest: {error}")
    raise SystemExit(1)

members = manifest.get("workspace", {}).get("members")
if not isinstance(members, list) or not members or not all(
    isinstance(member, str) for member in members
):
    fail(f"no non-empty [workspace] members list of strings in {manifest_path}")
    raise SystemExit(1)

try:
    with open(record_path, encoding="utf-8") as handle:
        record = json.load(handle)
except (OSError, UnicodeDecodeError, json.JSONDecodeError) as error:
    fail(f"cannot read justification record: {error}")
    raise SystemExit(1)

if record.get("schema_version") != 1:
    fail(f"{record_path}: schema_version must be 1")
    raise SystemExit(1)

entries = record.get("members")
if not isinstance(entries, dict):
    fail(f"{record_path}: top-level 'members' must be an object")
    raise SystemExit(1)

REQUIRED_STRING_FIELDS = ("module_insufficient_because", "owns_artifact_or_lifecycle")
REQUIRED_LIST_FIELDS = ("isolates_dependencies", "expected_consumers")
OPTIONAL_STRING_FIELDS = ("breaks_cycle",)
ALL_FIELDS = (
    set(REQUIRED_STRING_FIELDS)
    | set(REQUIRED_LIST_FIELDS)
    | set(OPTIONAL_STRING_FIELDS)
    | {"grandfathered"}
)

# The issue names this exact non-argument: unclaimed ownership of a NIP does
# not show a MODULE is insufficient, because a module can be the sole
# semantic owner of a NIP, an event kind, or a pipeline stage just as well.
BANNED_INSUFFICIENCY_RE = re.compile(
    r"no\s+other\s+(package|crate)\s+owns", re.IGNORECASE
)

errors = []
member_set = set(members)

for member in members:
    entry = entries.get(member)
    if entry is None:
        errors.append(
            f"'{member}' has no entry in {record_path} -- add a justification "
            "(or \"grandfathered\": true) before it can join [workspace] members"
        )
        continue
    if not isinstance(entry, dict):
        errors.append(f"'{member}': entry must be an object")
        continue

    unknown = sorted(set(entry) - ALL_FIELDS)
    if unknown:
        errors.append(f"'{member}': unknown field(s) {unknown}")

    if entry.get("grandfathered") is True:
        extra = sorted(set(entry) - {"grandfathered"})
        if extra:
            errors.append(
                f"'{member}': a grandfathered entry carries no other field, "
                f"found {extra}"
            )
        continue

    for field in REQUIRED_STRING_FIELDS:
        value = entry.get(field)
        if not isinstance(value, str) or not value.strip():
            errors.append(f"'{member}': '{field}' must be a non-empty string")
            continue
        if field == "module_insufficient_because" and BANNED_INSUFFICIENCY_RE.search(value):
            errors.append(
                f"'{member}': '{field}' reduces to \"no other package owns "
                "this\" -- a module can be the sole semantic owner of a NIP, "
                "an event kind, or a pipeline stage; state why a MODULE is "
                "insufficient, not why the package slot is unclaimed"
            )

    for field in REQUIRED_LIST_FIELDS:
        value = entry.get(field)
        if not isinstance(value, list) or not all(
            isinstance(item, str) and item.strip() for item in value
        ):
            errors.append(f"'{member}': '{field}' must be a list of non-empty strings")
            continue
        if field == "expected_consumers" and not value:
            errors.append(f"'{member}': 'expected_consumers' must name at least one consumer")

    breaks_cycle = entry.get("breaks_cycle")
    if breaks_cycle is not None and (
        not isinstance(breaks_cycle, str) or not breaks_cycle.strip()
    ):
        errors.append(f"'{member}': 'breaks_cycle' must be a non-empty string when present")

for recorded in sorted(set(entries) - member_set):
    errors.append(
        f"'{recorded}' has a justification entry in {record_path} but is not "
        "a current [workspace] member -- delete the stale entry"
    )

if errors:
    for error in errors:
        fail(error)
    raise SystemExit(1)

print("workspace-membership-justification: ok")
PY
