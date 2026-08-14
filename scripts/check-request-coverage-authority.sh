#!/usr/bin/env bash
# #816: coverage authority belongs to one exact request/session/wire FIFO.
# EVENT-commit failure poisons only the owners the wire evidence can name;
# ordinary EOSE and NIP-77 completion both consume that state through one
# request-atomic persistence door. The process-wide store-degraded diagnostic
# is observational and must never become coverage policy.
set -euo pipefail

SCRIPT_PATH=${BASH_SOURCE[0]}
SCRIPT_DIR=${SCRIPT_PATH%/*}
[[ $SCRIPT_DIR != "$SCRIPT_PATH" ]] || SCRIPT_DIR=.
source "$SCRIPT_DIR/lib/require-commands.sh" || exit 2
require_commands git grep xargs || exit 2

ROOT=$(cd "$SCRIPT_DIR/.." && pwd)
cd "$ROOT"

fail() {
  echo "request-coverage-authority: $*" >&2
  exit 1
}

required_paths=(
  crates/nmp/src/core/attribution.rs
  crates/nmp/src/core/auth_transport.rs
  crates/nmp/src/core/query.rs
  crates/nmp/src/core/observation.rs
  crates/nmp-resolver/src/engine.rs
  crates/nmp-store/src/coverage.rs
  crates/nmp-store/src/lib.rs
  crates/nmp-store/src/redb_store/event_ops.rs
)
for path in "${required_paths[@]}"; do
  [[ -f $path ]] || fail "required authority path is missing: $path"
done

production_core=$(
  git ls-files 'crates/nmp/src/core/*.rs' |
    grep -E '^crates/nmp/src/core/[^/]+\.rs$' |
    grep -vE '(_tests|auth_core_headless)\.rs$'
)
[[ -n $production_core ]] || fail "production core census is empty"

# Exactly two production completion callers exist: ordinary EOSE and NEG.
# Both must call the same helper; adding a third policy door or deleting one
# of the two required paths fails this exact census.
completion_calls=$(
  # shellcheck disable=SC2086
  printf '%s\n' "$production_core" |
    xargs grep -nF '.persist_attributed_completion(' || true
)
completion_count=$(
  printf '%s\n' "$completion_calls" |
    grep -c . || true
)
[[ $completion_count -eq 2 ]] || {
  printf '%s\n' "$completion_calls"
  fail "expected exactly two attributed-completion callers, found $completion_count"
}
printf '%s\n' "$completion_calls" |
  grep -qF 'crates/nmp/src/core/auth_transport.rs:' ||
  fail "ordinary EOSE no longer uses persist_attributed_completion"
printf '%s\n' "$completion_calls" |
  grep -qF 'crates/nmp/src/core/query.rs:' ||
  fail "NIP-77 completion no longer uses persist_attributed_completion"

# Only the shared helper may cross the RedbStore coverage-write boundary.
coverage_calls=$(
  # shellcheck disable=SC2086
  printf '%s\n' "$production_core" |
    xargs grep -nF '.record_coverage(' || true
)
coverage_count=$(
  printf '%s\n' "$coverage_calls" |
    grep -c . || true
)
[[ $coverage_count -eq 1 ]] || {
  printf '%s\n' "$coverage_calls"
  fail "expected one production RedbStore coverage-write call, found $coverage_count"
}
printf '%s\n' "$coverage_calls" |
  grep -qF 'crates/nmp/src/core/query.rs:' ||
  fail "the sole coverage-write call moved outside the shared completion door"

# `store_degraded` may be declared, projected, and first-error-latched for
# diagnostics. Any occurrence in the request/coverage policy owners is a
# process-global coverage gate and therefore a regression.
if grep -nF 'store_degraded' \
  crates/nmp/src/core/attribution.rs \
  crates/nmp/src/core/auth_transport.rs \
  crates/nmp/src/core/query.rs \
  crates/nmp/src/core/observation.rs; then
  fail "request coverage policy reads the process-wide diagnostic latch"
fi
grep -qF 'snapshot.store_degraded = self.store_degraded.clone();' \
  crates/nmp/src/core/mod.rs ||
  fail "the diagnostic projection seam disappeared"
grep -qF 'if self.store_degraded.is_none()' crates/nmp/src/core/write.rs ||
  fail "the first-error diagnostic latch seam disappeared"

# The final mechanism is grounded in current durable docs and source. A
# deleted consult, obsolete crate topology, draft checkpoint, or superseded
# branch must not become architecture authority in these ownership files.
legacy_authority=$(
  grep -nE \
    'docs/consults/2026-07-11-fable-coverage-attribution\.md|nmp-engine|e0c075fb458fe16b94d45b5a6416544a1b90ade5|2e9e4cb76dc6206ae79a296739bbc564236be41d|fix/816-fail-closed-coverage' \
    crates/nmp/src/core/attribution.rs \
    crates/nmp/src/core/auth_transport.rs \
    crates/nmp/src/core/query.rs \
    crates/nmp-resolver/src/engine.rs \
    crates/nmp-store/src/coverage.rs \
    crates/nmp-store/src/redb_store/event_ops.rs ||
    true
)
if [[ -n $legacy_authority ]]; then
  printf '%s\n' "$legacy_authority"
  fail "obsolete checkpoint/topology text is acting as mechanism authority"
fi

echo "request-coverage-authority: ok"
