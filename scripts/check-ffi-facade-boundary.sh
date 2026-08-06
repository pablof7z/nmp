#!/usr/bin/env bash
# #851: `nmp-ffi` consumes engine, query, receipt, signer and typed write
# values through the canonical `nmp` product facade. The mechanism crates
# behind it stay transitive implementation detail.
#
# #1239 extended that from the mechanism crates to every protocol and content
# family the facade offers. Those families used to be direct edges here and
# absent from the facade entirely, which made them reachable from Swift and
# unreachable from direct Rust; now that `nmp` offers each behind a feature, a
# direct edge here would be a second owner of exactly the values the facade
# projects. The one family still edged directly is `nmp-nip02`, which depends
# on `nmp` itself and so cannot become a facade dependency without a cycle.
#
# Why this is a mechanism and not a review convention: `nmp-ffi` is the ONE
# staticlib/cdylib every Swift and Kotlin app links. A direct import of a
# mechanism crate lets the native product boundary bind a type the canonical
# facade never projected or governed, and leaves "direct imports and facade
# re-exports agree" as something a reviewer has to remember. Re-adding
# `nmp-grammar.workspace = true` next to values `nmp` already re-exports is a
# one-line, entirely plausible edit; this script is what stops it.
#
# The primary check is the MANIFEST, not a source census, because the manifest
# is what the compiler enforces: with `nmp-grammar`/`nmp-signer`/`nmp-nip22`
# absent from `[dependencies]`, `cargo build -p nmp-ffi` cannot resolve
# `nmp_grammar::`/`nmp_signer::`/`nmp_nip22::` at all. The shipped artifact
# therefore binds nothing from them as a matter of compilation, not of taste.
#
# What remains is a RATCHET over what `#[cfg(test)]` code may still reach,
# since dev-dependencies are outside the compiler's production guarantee. The
# list below is exact, no entry may join it, and -- critically -- an entry that
# stops being reached FAILS, so the list can only shrink. It is debt, never a
# licence.
set -euo pipefail

SCRIPT_PATH=${BASH_SOURCE[0]}
SCRIPT_DIR=${SCRIPT_PATH%/*}
[[ $SCRIPT_DIR != "$SCRIPT_PATH" ]] || SCRIPT_DIR=.
source "$SCRIPT_DIR/lib/require-commands.sh" || exit 2
require_commands awk dirname git grep xargs || exit 2

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

fail() { echo "ffi-facade-boundary: $*" >&2; exit 1; }

FFI_MANIFEST=crates/nmp-ffi/Cargo.toml
FACADE_MANIFEST=crates/nmp/Cargo.toml

# Every protocol/content family the facade owns behind a cargo feature, as
# `<crate>:<feature>:<module>`. `nmp-ffi` reaches each by turning the feature
# on -- never by a second edge -- so this one table drives all three checks
# below: the edge stays closed, the feature stays enabled, and the facade
# module that projects the vocabulary stays present.
FACADE_OWNED_FAMILIES=(
  nmp-nip18:nip18:nip18
  nmp-nip22:nip22:nip22
  nmp-nip25:nip25:nip25
  nmp-nip51:nip51:nip51
  nmp-nipc7:nipc7:nipc7
  nmp-asset:asset:asset
  nmp-blossom:blossom:blossom
  nmp-content:content:content
)

# NIP-29 is owned by the facade UNCONDITIONALLY (`crates/nmp/src/nip29.rs` is a
# real door, not a re-export, so there is no mechanism crate for a feature to
# keep unlinked). The edge must still stay closed here.
FACADE_OWNED_UNCONDITIONAL=(nmp-nip29:nip29)

# Every path this gate reasons about must exist. A missing one would otherwise
# turn the searches below into a vacuous pass.
for required in "$FFI_MANIFEST" "$FACADE_MANIFEST" crates/nmp-ffi/src; do
  [[ -e $required ]] || fail "required path is missing: $required"
done
# A family's facade module is either a file or a directory (`nip29` is a real
# door with submodules, the rest are single-file re-export modules). Either
# spelling is the module existing; neither missing is acceptable, because an
# absent module would make the "reach it through the facade" checks vacuous.
for entry in "${FACADE_OWNED_FAMILIES[@]}" "${FACADE_OWNED_UNCONDITIONAL[@]}"; do
  module=${entry##*:}
  [[ -f crates/nmp/src/$module.rs || -d crates/nmp/src/$module ]] ||
    fail "facade module is missing: crates/nmp/src/$module"
done

# Portability note: plain POSIX `grep`/`awk` only. GitHub's ubuntu-latest
# runner has no `ripgrep`, and this gate must run with no toolchain and no
# setup step -- which is also why the DIRECT-edge check reads the manifest
# rather than shelling out to `cargo tree`. `[dependencies]` in Cargo.toml is
# the definition of a direct normal dependency edge; `cargo tree -p nmp-ffi
# -e normal --depth 1` is the same fact computed the slow way.
manifest_section() {
  awk -v want="$2" '
    /^[[:space:]]*\[/ { inside = ($0 == want); next }
    inside { print }
  ' "$1"
}

normal_deps=$(manifest_section "$FFI_MANIFEST" '[dependencies]')
[[ -n $normal_deps ]] || fail "$FFI_MANIFEST has no [dependencies] section to check"

# 1. The mechanism edges stay closed (#851: no production `nmp-grammar` or
#    `nmp-signer`), and so does every family the facade owns (#1239).
forbidden_edges=(nmp-grammar nmp-signer)
for entry in "${FACADE_OWNED_FAMILIES[@]}" "${FACADE_OWNED_UNCONDITIONAL[@]}"; do
  forbidden_edges+=("${entry%%:*}")
done
for forbidden in "${forbidden_edges[@]}"; do
  if printf '%s\n' "$normal_deps" |
    grep -qE "^[[:space:]]*${forbidden}[[:space:]]*[.=]"; then
    fail "nmp-ffi has a forbidden direct normal dependency: $forbidden"
  fi
done

printf '%s\n' "$normal_deps" | grep -qE '^[[:space:]]*nmp[[:space:]]*[.=]' ||
  fail "nmp-ffi is missing its canonical nmp dependency"

# 2. Every facade-owned family has ONE owner. `nmp-ffi` reaches each by turning
#    the facade's own feature on, never by a second edge.
#
#    The `nmp = ...` entry is read whole rather than line-by-line: an inline
#    table and a multi-line one are the same dependency, and a gate that only
#    understood one spelling would silently pass the other.
facade_dep_entry=$(printf '%s\n' "$normal_deps" | awk '
  /^[[:space:]]*nmp[[:space:]]*=/ { collecting = 1 }
  collecting {
    print
    depth += gsub(/\{/, "{") - gsub(/\}/, "}")
    if (depth <= 0) exit
  }
')
[[ -n $facade_dep_entry ]] || fail "could not read nmp-ffi's nmp dependency entry"

for entry in "${FACADE_OWNED_FAMILIES[@]}"; do
  crate=${entry%%:*}
  feature=${entry#*:}
  feature=${feature%%:*}
  printf '%s\n' "$facade_dep_entry" | grep -qE "\"${feature}\"" ||
    fail "nmp-ffi must enable the nmp/${feature} facade feature instead of edging $crate"
  grep -qE "^[[:space:]]*${feature}[[:space:]]*=" "$FACADE_MANIFEST" ||
    fail "$FACADE_MANIFEST is missing the ${feature} feature that owns $crate"
done

# 3. Source audit. Documentation may legitimately name the mechanism crate
#    whose value is being mirrored, so whole-line Rust comments (`//`, `///`,
#    `//!`) are excluded -- code, not prose, is what binds a second owner.
#
#    The searched corpus is `git ls-files` over crates/nmp-ffi's tracked RUST
#    sources, so build output and uniffi-generated bindings can neither hide a
#    violation nor manufacture one, and a `#`-comment in Cargo.toml naming a
#    residual symbol cannot keep a stale allowance alive. `xargs`'s "some
#    invocation exited 1" status differs between GNU and BSD, so a match is
#    detected by captured OUTPUT, never by exit status.
census() {
  git ls-files -- 'crates/nmp-ffi/*.rs' | xargs grep -nE "$1" 2>/dev/null || true
}
strip_comments() { grep -vE '^[^:]*:[0-9]+:[[:space:]]*//' || true; }

# No facade-owned family crate is a dependency of `nmp-ffi` in ANY section, so
# no source reference to one can be legitimate -- not even a test one.
for entry in "${FACADE_OWNED_FAMILIES[@]}" "${FACADE_OWNED_UNCONDITIONAL[@]}"; do
  crate=${entry%%:*}
  module=${entry##*:}
  underscored=$(printf '%s' "$crate" | tr - _)
  found=$(census "\\b${underscored}\\b" | strip_comments)
  if [[ -n $found ]]; then
    printf '%s\n' "$found"
    fail "nmp-ffi production source bypasses nmp::${module}"
  fi
done

# 4. `nmp-grammar` and `nmp-signer` survive only as DEV dependencies, which the
#    compiler does not police for the shipped artifact. Every `nmp_grammar::`/
#    `nmp_signer::` path in this crate's sources must therefore appear in the
#    exact list below. Any other one is a value the facade already projects, so
#    reaching for the mechanism crate re-opens a second owner.
#
#    Blocked on the facade snapshot ceiling (#934), NOT on design:
#      nmp_grammar::ConcreteFilter -- `nmp` re-exports `ShortfallFact`, whose
#      `NoPlannedSource`/`LocalLimit` variants carry an `atom: ConcreteFilter`
#      field that `nmp` does not project, so the acquisition-evidence
#      falsifier cannot build its fixture atom through the facade alone.
ALLOWED_RESIDUAL='nmp_grammar::ConcreteFilter'
found=$(census '\bnmp_(grammar|signer)::' |
  strip_comments |
  grep -vE "$ALLOWED_RESIDUAL" || true)
if [[ -n $found ]]; then
  printf '%s\n' "$found"
  fail "nmp-ffi imports a mechanism value the nmp facade already projects"
fi

# 5. Each residual entry must still be REACHED, or the list above has silently
#    become permission for something that no longer happens -- and the next
#    reader would read it as a licence rather than a debt. An entry that stops
#    being used must be deleted from this list in the same change. This is what
#    forced the earlier `nmp_grammar::reference` (deleted by #913) and the four
#    `nmp_signer::` NIP-46 entries off the list rather than letting them
#    linger as stale permission.
for residual in nmp_grammar::ConcreteFilter; do
  census "\\b${residual}\\b" | strip_comments | grep -q . ||
    fail "residual allowance is stale and must be removed: $residual"
done

echo "ffi-facade-boundary: ok"
