#!/usr/bin/env bash
# #877 link/resource inventory: the core artifact must contain zero concrete
# NIP-46 symbols or generated binding names, while the selected provider must
# contain both. Call after the relevant Swift or Kotlin build scripts.

set -euo pipefail

SCRIPT_PATH=${BASH_SOURCE[0]}
SCRIPT_DIR=${SCRIPT_PATH%/*}
[[ $SCRIPT_DIR != "$SCRIPT_PATH" ]] || SCRIPT_DIR=.
source "$SCRIPT_DIR/lib/require-commands.sh" || exit 2
require_commands find grep mktemp rm tr uname wc xargs || exit 2

if [[ $# -ne 4 ]]; then
  echo "usage: $0 CORE_LIBRARY PROVIDER_LIBRARY CORE_BINDINGS PROVIDER_BINDINGS" >&2
  exit 2
fi

CORE_LIBRARY=$1
PROVIDER_LIBRARY=$2
CORE_BINDINGS=$3
PROVIDER_BINDINGS=$4

fail() { echo "nip46-artifact-inventory: $*" >&2; exit 1; }

[[ -f "$CORE_LIBRARY" ]] || fail "core library is missing: $CORE_LIBRARY"
[[ -f "$PROVIDER_LIBRARY" ]] || fail "provider library is missing: $PROVIDER_LIBRARY"
[[ -d "$CORE_BINDINGS" ]] || fail "core binding directory is missing: $CORE_BINDINGS"
[[ -d "$PROVIDER_BINDINGS" ]] || fail "provider binding directory is missing: $PROVIDER_BINDINGS"

TMP=$(mktemp -d "${TMPDIR:-/tmp}/nmp-nip46-artifacts.XXXXXX")
trap 'rm -rf "$TMP"' EXIT

case "$(uname -s)" in
  Darwin)
    require_commands nm strings || exit 2
    # Apple nm can lag the LLVM object format shipped by the pinned nightly.
    # Prefer its defined-global map; fall back to the archive's string table,
    # which still contains the exported UniFFI symbol names.
    if ! nm -gU "$CORE_LIBRARY" > "$TMP/core-symbols" 2> "$TMP/core-nm-errors"; then
      strings -a "$CORE_LIBRARY" > "$TMP/core-symbols"
    fi
    if ! nm -gU "$PROVIDER_LIBRARY" > "$TMP/provider-symbols" 2> "$TMP/provider-nm-errors"; then
      strings -a "$PROVIDER_LIBRARY" > "$TMP/provider-symbols"
    fi
    ;;
  Linux)
    require_commands nm || exit 2
    nm -D --defined-only "$CORE_LIBRARY" > "$TMP/core-symbols"
    nm -D --defined-only "$PROVIDER_LIBRARY" > "$TMP/provider-symbols"
    ;;
  *)
    fail "unsupported symbol-map host: $(uname -s)"
    ;;
esac

# `nostr`'s protocol-neutral NIP-05 profile includes a JSON field literally
# named `nip46`; that is directory metadata, not the optional signer provider.
# Match the provider namespace/types/URI instead of overclaiming that unrelated
# field as linked provider code.
PATTERN='nmp[_-]nip46|FfiNip46|NmpNip46|Nip46(Signer|Connection|Invitation|Session|Provider)|Bunker(Parse|Uri)|bunker://'

core_symbol_count=$(grep -Eic "$PATTERN" "$TMP/core-symbols" || true)
provider_symbol_count=$(grep -Eic "$PATTERN" "$TMP/provider-symbols" || true)
[[ $core_symbol_count -eq 0 ]] || {
  grep -Ei "$PATTERN" "$TMP/core-symbols" >&2
  fail "core link map contains NIP-46 symbols"
}
[[ $provider_symbol_count -gt 0 ]] ||
  fail "selected provider link map contains no NIP-46 symbols"

binding_matches() {
  local root=$1
  (
    set +o pipefail
    find "$root" -type f \( -name '*.swift' -o -name '*.kt' -o -name '*.h' \) -print0 |
      xargs -0 grep -Eih "$PATTERN" 2>/dev/null |
      wc -l |
      tr -d ' '
  )
}

core_binding_count=$(binding_matches "$CORE_BINDINGS")
provider_binding_count=$(binding_matches "$PROVIDER_BINDINGS")
[[ $core_binding_count -eq 0 ]] ||
  fail "core generated bindings contain NIP-46 names"
[[ $provider_binding_count -gt 0 ]] ||
  fail "selected provider generated bindings contain no NIP-46 names"

echo "nip46-artifact-inventory: core-symbols=$core_symbol_count provider-symbols=$provider_symbol_count core-bindings=$core_binding_count provider-bindings=$provider_binding_count"
