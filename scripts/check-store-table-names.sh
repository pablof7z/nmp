#!/usr/bin/env bash
# Current Redb table names belong to nmp-store. Other crates open a store
# helper, not a table string. #1451.
set -euo pipefail

SCRIPT_PATH=${BASH_SOURCE[0]}
SCRIPT_DIR=${SCRIPT_PATH%/*}
[[ $SCRIPT_DIR != "$SCRIPT_PATH" ]] || SCRIPT_DIR=.
source "$SCRIPT_DIR/lib/require-commands.sh" || exit 2
require_commands git python3 || exit 2

ROOT=$(cd "$SCRIPT_DIR/.." && pwd)
cd "$ROOT"

fail() { echo "store-table-names: $*" >&2; exit 1; }

SCHEMA=crates/nmp-store/src/redb_store/schema.rs
[[ -f $SCHEMA ]] || fail "schema declaration is missing: $SCHEMA"

python3 - "$SCHEMA" <<'PY'
import re
import subprocess
import sys

schema_path = sys.argv[1]
names = re.findall(
    r'TableDefinition::new\("([^"]+)"\)',
    open(schema_path, encoding="utf-8").read(),
)
if not names:
    print("store-table-names: no table names in schema.rs", file=sys.stderr)
    raise SystemExit(1)

tracked = subprocess.check_output(["git", "ls-files", "-z"], text=True).split("\0")
tracked = [
    path
    for path in tracked
    if path
    and not path.startswith("crates/nmp-store/")
    and path.endswith(".rs")
]

hits = []
open_re = re.compile(
    r'TableDefinition(?:\s*::\s*new|\s*<[^>]*>\s*::\s*new)\s*\(\s*"([^"]+)"'
)
for path in tracked:
    try:
        text = open(path, encoding="utf-8").read()
    except (OSError, UnicodeDecodeError):
        continue
    for match in open_re.finditer(text):
        if match.group(1) in names:
            hits.append(f'{path}: TableDefinition::new("{match.group(1)}")')

# Unique, stable order.
seen = []
for hit in hits:
    if hit not in seen:
        seen.append(hit)

if seen:
    print("store-table-names: current table names escaped nmp-store:", file=sys.stderr)
    print("\n".join(seen), file=sys.stderr)
    raise SystemExit(1)

print("store-table-names: ok")
PY
