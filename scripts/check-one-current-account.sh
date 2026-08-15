#!/usr/bin/env bash
# The current account is stored exactly once. #1657.
#
# `EngineCore.active_pubkey` is the one copy. The reducer must hold it: it
# resolves `Identity::Active` and re-roots reactive bindings from pure
# `&mut self` code that cannot reach the runtime's account registry, and
# `EngineCore` is exercised headlessly with no runtime in existence at all.
#
# `crates/nmp/src/runtime/` therefore stores no second copy. It used to, and
# the two were kept equal by six pairs of adjacent statements that nothing
# typed -- deleting either half still compiled, while the sign-event author
# check read one copy and reactive re-rooting read the other. A divergence
# would have signed as one account while the reducer authored as another.
#
# This refuses a reintroduced field declaration. It does not refuse reads
# (`core.active_pubkey()`) or the `SessionSnapshot.current_pubkey` value the
# runtime builds from the reducer's answer -- those are the correct shape.
set -euo pipefail

SCRIPT_PATH=${BASH_SOURCE[0]}
SCRIPT_DIR=${SCRIPT_PATH%/*}
[[ $SCRIPT_DIR != "$SCRIPT_PATH" ]] || SCRIPT_DIR=.
source "$SCRIPT_DIR/lib/require-commands.sh" || exit 2
require_commands git grep || exit 2

ROOT=$(cd "$SCRIPT_DIR/.." && pwd)
cd "$ROOT"

RUNTIME=crates/nmp/src/runtime
[[ -d $RUNTIME ]] || {
    echo "one-current-account: runtime source is missing: $RUNTIME" >&2
    exit 1
}

# A stored copy is a struct FIELD DECLARATION: `<name>: Option<PublicKey>`.
# A struct literal's `current_pubkey: current,` and a call's
# `core.active_pubkey()` both fail to match, which is what keeps this gate
# from refusing the correct shape.
if hits=$(grep -RIn --include='*.rs' \
    -E '^[[:space:]]*(pub(\([^)]*\))?[[:space:]]+)?(current_pubkey|active_pubkey)[[:space:]]*:[[:space:]]*Option[[:space:]]*<' \
    "$RUNTIME"); then
    echo "one-current-account: the runtime reintroduced a current-account field:" >&2
    echo "$hits" >&2
    echo >&2
    echo "The one copy is EngineCore.active_pubkey; read it with core.active_pubkey()." >&2
    exit 1
fi

# The reducer's copy must still exist, or this gate would pass vacuously on a
# codebase that had deleted the owner too.
if ! grep -qE '^[[:space:]]*active_pubkey[[:space:]]*:[[:space:]]*Option[[:space:]]*<' \
    crates/nmp/src/core/mod.rs; then
    echo "one-current-account: EngineCore.active_pubkey is gone; this gate no longer measures anything." >&2
    exit 1
fi

echo "one-current-account: ok (EngineCore.active_pubkey is the only stored copy)"
