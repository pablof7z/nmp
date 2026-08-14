#!/usr/bin/env bash
# Falsifiers for #1559's workspace-membership existence gate: proves both
# directions the issue asks for -- a new member with no justification record
# fails, and one with a complete record passes -- plus the explicit "no other
# package owns this NIP" refusal and the tool-requirements contract.

set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
CHECKER="$ROOT/scripts/check-workspace-membership-justification.sh"
BASH_BIN=$(command -v bash)
TEMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/nmp-workspace-membership-test.XXXXXX")
trap 'rm -rf "$TEMP_ROOT"' EXIT

fail() {
  echo "workspace-membership-justification test: $*" >&2
  exit 1
}

write_fixture() {
  # write_fixture DIR MEMBER1 MEMBER2 ...
  local dir=$1
  shift
  mkdir -p "$dir"
  {
    echo '[workspace]'
    echo 'resolver = "2"'
    echo 'members = ['
    local member
    for member in "$@"; do
      printf '  "%s",\n' "$member"
    done
    echo ']'
  } >"$dir/Cargo.toml"
}

write_record() {
  # write_record DIR JSON_BODY
  local dir=$1
  local body=$2
  mkdir -p "$dir"
  printf '%s' "$body" >"$dir/justifications.json"
}

run_checker() {
  local dir=$1
  "$BASH_BIN" "$CHECKER" "$dir/Cargo.toml" "$dir/justifications.json"
}

expect_pass() {
  local label=$1
  local dir=$2
  run_checker "$dir" >/dev/null || fail "$label unexpectedly failed"
}

expect_fail() {
  local label=$1
  local dir=$2
  shift 2
  local output
  if output=$(run_checker "$dir" 2>&1); then
    fail "$label unexpectedly passed"
  fi
  local expected
  for expected in "$@"; do
    grep -Fq -- "$expected" <<<"$output" ||
      fail "$label failed for the wrong reason; missing '$expected': $output"
  done
}

# The live repository record passes as shipped.
"$BASH_BIN" "$CHECKER" >/dev/null || fail "live repository record did not pass"

# The wrapper's one external prerequisite is removed and must exit 2 naming
# exactly that tool -- python3 is required by the checker itself, so bash is
# the only other tool an isolated PATH may still resolve.
isolated_path="$TEMP_ROOT/path-without-python3"
mkdir "$isolated_path"
ln -s "$(command -v bash)" "$isolated_path/bash"
set +e
missing_output=$(PATH="$isolated_path" "$BASH_BIN" "$CHECKER" 2>&1)
missing_status=$?
set -e
[[ $missing_status -eq 2 ]] ||
  fail "missing python3 exited $missing_status instead of 2"
expected_missing="check-tools: required command(s) unavailable: python3"
[[ $missing_output == "$expected_missing" ]] ||
  fail "missing python3 produced the wrong refusal: $missing_output"

# 1. A new member with no justification entry at all fails closed.
undocumented="$TEMP_ROOT/undocumented"
write_fixture "$undocumented" crates/nmp-store crates/nmp-nip99
write_record "$undocumented" '{
  "schema_version": 1,
  "members": {
    "crates/nmp-store": { "grandfathered": true }
  }
}'
expect_fail "undocumented new member" "$undocumented" \
  "'crates/nmp-nip99' has no entry in" \
  "add a justification"

# 2. The same new member, with a complete justification record, passes.
documented="$TEMP_ROOT/documented"
write_fixture "$documented" crates/nmp-store crates/nmp-nip99
write_record "$documented" '{
  "schema_version": 1,
  "members": {
    "crates/nmp-store": { "grandfathered": true },
    "crates/nmp-nip99": {
      "module_insufficient_because": "it owns a durable background reconciliation loop with its own crash-recovery lifecycle that a module inside crates/nmp cannot host without pulling the whole facade into a tokio task it does not otherwise need",
      "isolates_dependencies": ["nip99-upstream-sdk"],
      "owns_artifact_or_lifecycle": "the standalone nip99-reconciler binary shipped to relay operators",
      "expected_consumers": ["crates/nmp-cli"],
      "breaks_cycle": null
    }
  }
}'
expect_pass "documented new member" "$documented"

# 3. "No other package owns this NIP" is refused explicitly, even with the
#    other fields present and well-formed.
bare_ownership="$TEMP_ROOT/bare-ownership"
write_fixture "$bare_ownership" crates/nmp-store crates/nmp-nip99
write_record "$bare_ownership" '{
  "schema_version": 1,
  "members": {
    "crates/nmp-store": { "grandfathered": true },
    "crates/nmp-nip99": {
      "module_insufficient_because": "no other package owns this NIP",
      "isolates_dependencies": [],
      "owns_artifact_or_lifecycle": "NIP-99 kind:99 events",
      "expected_consumers": ["crates/nmp-cli"]
    }
  }
}'
expect_fail "bare NIP-ownership justification" "$bare_ownership" \
  "reduces to \"no other package owns this\"" \
  "a module can be the sole semantic owner"

# 4. A grandfathered entry with an extra field is refused -- it is not a
#    place to sneak in an unreviewed justification.
grandfathered_extra="$TEMP_ROOT/grandfathered-extra"
write_fixture "$grandfathered_extra" crates/nmp-store
write_record "$grandfathered_extra" '{
  "schema_version": 1,
  "members": {
    "crates/nmp-store": {
      "grandfathered": true,
      "owns_artifact_or_lifecycle": "should not be here"
    }
  }
}'
expect_fail "grandfathered entry with extra field" "$grandfathered_extra" \
  "a grandfathered entry carries no other field"

# 5. expected_consumers must name at least one consumer.
no_consumers="$TEMP_ROOT/no-consumers"
write_fixture "$no_consumers" crates/nmp-store crates/nmp-nip99
write_record "$no_consumers" '{
  "schema_version": 1,
  "members": {
    "crates/nmp-store": { "grandfathered": true },
    "crates/nmp-nip99": {
      "module_insufficient_because": "it ships an independent release artifact with its own versioned CLI surface",
      "isolates_dependencies": [],
      "owns_artifact_or_lifecycle": "the nip99 export tool",
      "expected_consumers": []
    }
  }
}'
expect_fail "no expected consumers" "$no_consumers" \
  "'expected_consumers' must name at least one consumer"

# 6. A justification entry for a member that already left the workspace is
#    stale and refused, the same "no compatibility surface" posture #1559
#    itself is built on.
stale="$TEMP_ROOT/stale"
write_fixture "$stale" crates/nmp-store
write_record "$stale" '{
  "schema_version": 1,
  "members": {
    "crates/nmp-store": { "grandfathered": true },
    "crates/nmp-retired": { "grandfathered": true }
  }
}'
expect_fail "stale justification entry" "$stale" \
  "'crates/nmp-retired' has a justification entry" \
  "delete the stale entry"

echo "workspace-membership-justification test: ok"
