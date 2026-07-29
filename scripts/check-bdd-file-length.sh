#!/usr/bin/env bash
# A 600-line hard cap per file, enforced for `crates/nmp-bdd/**` only.
#
# WHY THIS EXISTS. The cap was already the repository's norm, and a norm is
# exactly how `crates/nmp-bdd/src/world.rs` reached 1,333 lines: nothing ever
# rejected the commit that took it from 590 to 610, and no single commit
# afterwards looked like the one that made it unreadable. This script is the
# mechanism, in the same "plain shell over tracked repo content, no toolchain,
# no secrets" character as the other checks in
# .github/workflows/architecture-gates.yml.
#
# WHY ONLY nmp-bdd. Measured at the time of writing, 97 of the workspace's 299
# tracked `.rs` files are over 600 lines (`crates/nmp/src/runtime/mod.rs` alone
# is 6,499). A workspace-wide gate would therefore fail on day one and would
# have to be introduced with a grandfather list -- which is a different and
# much larger decision than this one. `crates/nmp-bdd` passes today with room
# to spare (its largest file is 431 lines), so this scope can be enforced
# honestly and immediately. Widening it later is a matter of changing SCOPE
# below, once the newly covered tree actually passes.
#
# WHY A LINE COUNT AT ALL. It is a proxy, and a crude one -- a file can be
# unreadable at 300 lines and fine at 700. What it buys is a forcing function:
# when the cap is hit, SOMEBODY has to name the seam. The failure message says
# so rather than inviting a mechanical cut at line 600.
set -euo pipefail

SCRIPT_PATH=${BASH_SOURCE[0]}
SCRIPT_DIR=${SCRIPT_PATH%/*}
[[ $SCRIPT_DIR != "$SCRIPT_PATH" ]] || SCRIPT_DIR=.
source "$SCRIPT_DIR/lib/require-commands.sh" || exit 2
require_commands cat dirname git tr wc || exit 2

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

# The trees this cap applies to. Tracked files only (`git ls-files`), so build
# output and generated bindings can neither hide a violation nor manufacture
# one.
SCOPE=(crates/nmp-bdd)

MAX=600

# A missing scope would turn this gate into a vacuous pass.
for path in "${SCOPE[@]}"; do
  [[ -d $path ]] || {
    echo "bdd-file-length: scoped path is missing: $path" >&2
    exit 1
  }
done

violations=""
while IFS= read -r file; do
  [[ -f $file ]] || continue
  # BSD `wc` pads its output; strip the padding so the report reads cleanly.
  lines=$(wc -l <"$file" | tr -d '[:space:]')
  if ((lines > MAX)); then
    violations+="  $file: $lines lines"$'\n'
  fi
done < <(git ls-files -- "${SCOPE[@]}")

if [[ -n $violations ]]; then
  echo "bdd-file-length: file(s) over the ${MAX}-line cap:" >&2
  printf '%s' "$violations" >&2
  cat >&2 <<'EOF'

Do not cut at line 600. Find the seam: the concern this file has quietly
taken on a second copy of, and the module boundary that would let a reader
find it by name. See crates/nmp-bdd/src/world/mod.rs for how the world's
own split is stated -- each module says what it owns and why that boundary
is the right one.
EOF
  exit 1
fi

echo "bdd-file-length: ok (no file in ${SCOPE[*]} over ${MAX} lines)"
