#!/usr/bin/env bash
# #851: `nmp-ffi` consumes engine, query, receipt, signer and typed write
# values through the canonical `nmp` product facade. The mechanism crates
# behind it stay transitive implementation detail.
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

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

fail() { echo "ffi-facade-boundary: $*" >&2; exit 1; }

FFI_MANIFEST=crates/nmp-ffi/Cargo.toml
FACADE_MANIFEST=crates/nmp/Cargo.toml
FACADE_NIP22=crates/nmp/src/nip22.rs

# Every path this gate reasons about must exist. A missing one would otherwise
# turn the searches below into a vacuous pass.
for required in "$FFI_MANIFEST" "$FACADE_MANIFEST" "$FACADE_NIP22" crates/nmp-ffi/src; do
  [[ -e $required ]] || fail "required path is missing: $required"
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

# 1. The mechanism edges stay closed. This is the whole of #851's required
#    target: no production `nmp-grammar`, `nmp-signer` or `nmp-nip22` edge.
for forbidden in nmp-grammar nmp-signer nmp-nip22; do
  if printf '%s\n' "$normal_deps" |
    grep -qE "^[[:space:]]*${forbidden}[[:space:]]*[.=]"; then
    fail "nmp-ffi has a forbidden direct normal dependency: $forbidden"
  fi
done

printf '%s\n' "$normal_deps" | grep -qE '^[[:space:]]*nmp[[:space:]]*[.=]' ||
  fail "nmp-ffi is missing its canonical nmp dependency"

# 2. The NIP-22 comment vocabulary has ONE owner. `nmp-ffi` reaches it by
#    turning the facade's own feature on, never by a second edge.
printf '%s\n' "$normal_deps" |
  grep -E '^[[:space:]]*nmp[[:space:]]*[.=]' | grep -qF 'nip22' ||
  fail "nmp-ffi must enable the nmp/nip22 facade feature to project comments"
grep -qE '^[[:space:]]*nip22[[:space:]]*=' "$FACADE_MANIFEST" ||
  fail "$FACADE_MANIFEST is missing the nip22 feature that owns NIP-22"

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

# `nmp-nip22` is not a dependency of `nmp-ffi` in ANY section, so no source
# reference to it can be legitimate -- not even a test one.
found=$(census '\bnmp_nip22\b' | strip_comments)
if [[ -n $found ]]; then
  printf '%s\n' "$found"
  fail "nmp-ffi production source bypasses nmp::nip22"
fi

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
#    `nmp_signer::` NIP-46 entries (moved out to `nmp-nip46-ffi` by #945) off
#    the list rather than letting them linger as stale permission.
for residual in nmp_grammar::ConcreteFilter; do
  census "\\b${residual}\\b" | strip_comments | grep -q . ||
    fail "residual allowance is stale and must be removed: $residual"
done

echo "ffi-facade-boundary: ok"
