#!/usr/bin/env bash
# Non-vacuous graph, classification, determinism, and wrapper falsifiers for
# #922's dependency-direction gate.

set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
DEFAULT_CHECKER="$ROOT/scripts/check-dependency-direction.sh"
VALIDATOR="$ROOT/scripts/check-dependency-direction.py"
POLICY="$ROOT/scripts/dependency-direction-policy.json"
BASH_BIN=$(command -v bash)
TEMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/nmp-dependency-direction-test.XXXXXX")
trap 'rm -rf "$TEMP_ROOT"' EXIT

MODE=full
CHECKER=$DEFAULT_CHECKER
if [[ ${1:-} == "--optional-only" ]]; then
  [[ $# -eq 2 ]] || {
    echo "dependency-direction test: --optional-only requires a checker" >&2
    exit 2
  }
  MODE=optional-only
  CHECKER=$2
elif [[ $# -ne 0 ]]; then
  echo "dependency-direction test: usage: $0 [--optional-only CHECKER]" >&2
  exit 2
fi

fail() {
  echo "dependency-direction test: $*" >&2
  exit 1
}

write_workspace() {
  local workspace=$1
  shift
  mkdir -p "$workspace"
  {
    echo '[workspace]'
    echo 'resolver = "2"'
    echo 'members = ['
    local member
    for member in "$@"; do
      printf '  "%s",\n' "$member"
    done
    echo ']'
  } >"$workspace/Cargo.toml"
}

write_crate() {
  local workspace=$1
  local name=$2
  local extra=${3:-}
  mkdir -p "$workspace/$name/src"
  {
    echo '[package]'
    printf 'name = "%s"\n' "$name"
    echo 'version = "0.1.0"'
    echo 'edition = "2021"'
    if [[ -n $extra ]]; then
      printf '%s\n' "$extra"
    fi
  } >"$workspace/$name/Cargo.toml"
  printf 'pub fn fixture() {}\n' >"$workspace/$name/src/lib.rs"
}

run_checker() {
  local workspace=$1
  "$BASH_BIN" "$CHECKER" --unlocked "$workspace/Cargo.toml"
}

expect_failure() {
  local label=$1
  local workspace=$2
  shift 2
  local output
  if output=$(run_checker "$workspace" 2>&1); then
    fail "$label unexpectedly passed"
  fi
  local expected
  for expected in "$@"; do
    grep -Fq -- "$expected" <<<"$output" ||
      fail "$label failed for the wrong reason; missing '$expected': $output"
  done
}

expect_validator_failure() {
  local label=$1
  local policy=$2
  local metadata=$3
  shift 3
  local output
  if output=$(python3 "$VALIDATOR" "$policy" "$metadata" 2>&1); then
    fail "$label unexpectedly passed"
  fi
  local expected
  for expected in "$@"; do
    grep -Fq -- "$expected" <<<"$output" ||
      fail "$label failed for the wrong reason; missing '$expected': $output"
  done
}

write_optional_fixture() {
  local optional=$1
  write_workspace "$optional" nmp-nip99 nmp-store
  write_crate "$optional" nmp-store
  local optional_manifest
  optional_manifest=$'[features]\ndefault = []\n'
  optional_manifest+=$'forbidden = ["dep:nmp-store"]\n\n'
  optional_manifest+=$'[dependencies]\n'
  optional_manifest+=$'nmp-store = { path = "../nmp-store", optional = true }'
  write_crate "$optional" nmp-nip99 "$optional_manifest"
}

test_optional_edge() {
  local optional="$TEMP_ROOT/non-default-feature"
  write_optional_fixture "$optional"
  local default_metadata="$optional/default-metadata.json"
  cargo metadata \
    --format-version 1 \
    --manifest-path "$optional/Cargo.toml" >"$default_metadata"
  python3 "$VALIDATOR" "$POLICY" "$default_metadata" >/dev/null ||
    fail "the non-default fixture is invalid without its optional feature"
  expect_failure \
    "non-default feature forbidden edge" \
    "$optional" \
    "shortest path: nmp-nip99 -> nmp-store"
}

if [[ $MODE == optional-only ]]; then
  test_optional_edge
  echo "dependency-direction test: optional edge refused"
  exit 0
fi

# The live locked all-features graph passes without an enrollment manifest.
"$BASH_BIN" "$CHECKER" >/dev/null

# Every wrapper prerequisite is removed independently. The real wrapper must
# exit 2 and name precisely the tool that is absent.
for missing_tool in cargo python3 mktemp rm; do
  isolated_path="$TEMP_ROOT/path-without-$missing_tool"
  mkdir "$isolated_path"
  for available_tool in bash cargo python3 mktemp rm; do
    [[ $available_tool == "$missing_tool" ]] && continue
    ln -s "$(command -v "$available_tool")" "$isolated_path/$available_tool"
  done
  set +e
  missing_output=$(
    PATH="$isolated_path" "$BASH_BIN" "$DEFAULT_CHECKER" 2>&1
  )
  missing_status=$?
  set -e
  [[ $missing_status -eq 2 ]] ||
    fail "missing $missing_tool exited $missing_status instead of 2"
  expected_missing="check-tools: required command(s) unavailable: $missing_tool"
  [[ $missing_output == "$expected_missing" ]] ||
    fail "missing $missing_tool produced the wrong refusal: $missing_output"
done

# A new family package is classified without an exact exception and may reach
# the two generic value packages.
allowed="$TEMP_ROOT/allowed-family"
write_workspace "$allowed" nmp-nip99 nmp-grammar nmp-event-edit
write_crate "$allowed" nmp-event-edit
write_crate "$allowed" nmp-grammar \
  $'[dependencies]\nnmp-event-edit = { path = "../nmp-event-edit" }'
write_crate "$allowed" nmp-nip99 \
  $'[dependencies]\nnmp-grammar = { path = "../nmp-grammar" }'
run_checker "$allowed" >/dev/null

# The NIP-29 proof is deliberately about rule origin as well as role. Adding an
# exact exception with the same role must therefore fail.
nip29="$TEMP_ROOT/nip29-family-origin"
write_workspace "$nip29" nmp-nip29 nmp-grammar nmp-event-edit
write_crate "$nip29" nmp-event-edit
write_crate "$nip29" nmp-grammar \
  $'[dependencies]\nnmp-event-edit = { path = "../nmp-event-edit" }'
write_crate "$nip29" nmp-nip29 \
  $'[dependencies]\nnmp-grammar = { path = "../nmp-grammar" }'
nip29_output=$(run_checker "$nip29")
grep -Fq \
  "focused classification: nmp-nip29 [pure-protocol] via family rule 'nmp-nip'" \
  <<<"$nip29_output" ||
  fail "NIP-29 did not report its family classification origin"

nip29_metadata="$TEMP_ROOT/nip29-metadata.json"
cargo metadata \
  --format-version 1 \
  --all-features \
  --manifest-path "$nip29/Cargo.toml" >"$nip29_metadata"
exact_mutant="$TEMP_ROOT/policy-exact-nip29.json"
python3 - "$POLICY" "$exact_mutant" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    policy = json.load(source)
policy["role_rules"]["exact"]["nmp-nip29"] = "pure-protocol"
with open(sys.argv[2], "w", encoding="utf-8") as target:
    json.dump(policy, target)
PY
expect_validator_failure \
  "NIP-29 exact-exception mutation" \
  "$exact_mutant" \
  "$nip29_metadata" \
  "must be pure-protocol via family rule 'nmp-nip'" \
  "got pure-protocol via exact rule 'nmp-nip29'"

# The NIP-02 facade stop is a graph exception only. It permits the facade path
# while a direct mechanism edge remains forbidden.
service_allowed="$TEMP_ROOT/service-through-facade"
write_workspace "$service_allowed" nmp-nip02 nmp nmp-store
write_crate "$service_allowed" nmp-store
write_crate "$service_allowed" nmp \
  $'[dependencies]\nnmp-store = { path = "../nmp-store" }'
write_crate "$service_allowed" nmp-nip02 \
  $'[dependencies]\nnmp = { path = "../nmp" }'
run_checker "$service_allowed" >/dev/null

service_direct="$TEMP_ROOT/service-direct-mechanism"
write_workspace "$service_direct" nmp-nip02 nmp nmp-store
write_crate "$service_direct" nmp-store
write_crate "$service_direct" nmp \
  $'[dependencies]\nnmp-store = { path = "../nmp-store" }'
write_crate "$service_direct" nmp-nip02 \
  $'[dependencies]\nnmp = { path = "../nmp" }\nnmp-store = { path = "../nmp-store" }'
expect_failure \
  "protocol service direct mechanism edge" \
  "$service_direct" \
  "shortest path: nmp-nip02 -> nmp-store"

direct="$TEMP_ROOT/direct"
write_workspace "$direct" nmp-nip99 nmp-store
write_crate "$direct" nmp-store
write_crate "$direct" nmp-nip99 \
  $'[dependencies]\nnmp-store = { path = "../nmp-store" }'
expect_failure \
  "direct forbidden edge" \
  "$direct" \
  "source: nmp-nip99 [pure-protocol; family rule 'nmp-nip']" \
  "forbidden target: nmp-store [generic-mechanism; exact rule 'nmp-store']" \
  "shortest path: nmp-nip99 -> nmp-store"

# An unclassified intermediary cannot hide a path. A separate longer route is
# present so the shortest-path assertion is non-vacuous.
transitive="$TEMP_ROOT/transitive"
write_workspace \
  "$transitive" \
  nmp-nip99 \
  bridge \
  detour \
  long-bridge \
  nmp-store
write_crate "$transitive" nmp-store
write_crate "$transitive" bridge \
  $'[dependencies]\nnmp-store = { path = "../nmp-store" }'
write_crate "$transitive" long-bridge \
  $'[dependencies]\nnmp-store = { path = "../nmp-store" }'
write_crate "$transitive" detour \
  $'[dependencies]\nlong-bridge = { path = "../long-bridge" }'
write_crate "$transitive" nmp-nip99 \
  $'[dependencies]\nbridge = { path = "../bridge" }\ndetour = { path = "../detour" }'
expect_failure \
  "transitive forbidden edge" \
  "$transitive" \
  "shortest path: nmp-nip99 -> bridge -> nmp-store"

generic="$TEMP_ROOT/generic-to-protocol"
write_workspace "$generic" nmp-router nmp-nip99
write_crate "$generic" nmp-nip99
write_crate "$generic" nmp-router \
  $'[dependencies]\nnmp-nip99 = { path = "../nmp-nip99" }'
expect_failure \
  "generic mechanism to protocol" \
  "$generic" \
  "shortest path: nmp-router -> nmp-nip99"

# The focused NIP-29 fixture remains family-classified and is rejected through
# the role policy when it reaches a remaining generic mechanism. Reclassifying
# that target to a newly invented future role in a policy copy is rejected
# without adding a second forbidden-role list.
nip29_forbidden="$TEMP_ROOT/nip29-to-store"
write_workspace "$nip29_forbidden" nmp-nip29 nmp-store
write_crate "$nip29_forbidden" nmp-store
write_crate "$nip29_forbidden" nmp-nip29 \
  $'[dependencies]\nnmp-store = { path = "../nmp-store" }'
expect_failure \
  "NIP-29 family-preserving forbidden edge" \
  "$nip29_forbidden" \
  "focused classification: nmp-nip29 [pure-protocol] via family rule 'nmp-nip'" \
  "shortest path: nmp-nip29 -> nmp-store"

nip29_forbidden_metadata="$TEMP_ROOT/nip29-forbidden-metadata.json"
cargo metadata \
  --format-version 1 \
  --all-features \
  --manifest-path "$nip29_forbidden/Cargo.toml" \
  >"$nip29_forbidden_metadata"
future_role_policy="$TEMP_ROOT/policy-future-role.json"
python3 - "$POLICY" "$future_role_policy" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    policy = json.load(source)
policy["roles"]["future-runtime"] = {
    "description": "Synthetic future role used only by the falsifier.",
    "may_reach": ["generic-value"],
}
policy["role_rules"]["exact"]["nmp-store"] = "future-runtime"
with open(sys.argv[2], "w", encoding="utf-8") as target:
    json.dump(policy, target)
PY
expect_validator_failure \
  "NIP-29 future-role derivation" \
  "$future_role_policy" \
  "$nip29_forbidden_metadata" \
  "forbidden target: nmp-store [future-runtime; exact rule 'nmp-store']"

test_optional_edge

# The self-test must kill the exact mutation that drops all-feature resolution.
mutant_root="$TEMP_ROOT/all-features-mutant"
mkdir -p "$mutant_root/scripts/lib"
cp "$DEFAULT_CHECKER" "$mutant_root/scripts/check-dependency-direction.sh"
cp "$VALIDATOR" "$mutant_root/scripts/check-dependency-direction.py"
cp "$POLICY" "$mutant_root/scripts/dependency-direction-policy.json"
cp \
  "$ROOT/scripts/lib/require-commands.sh" \
  "$mutant_root/scripts/lib/require-commands.sh"
sed '/--all-features/d' \
  "$mutant_root/scripts/check-dependency-direction.sh" \
  >"$mutant_root/scripts/check-dependency-direction-mutant.sh"
mv \
  "$mutant_root/scripts/check-dependency-direction-mutant.sh" \
  "$mutant_root/scripts/check-dependency-direction.sh"
mutant_log="$TEMP_ROOT/all-features-mutant.log"
if "$BASH_BIN" "$0" \
  --optional-only "$mutant_root/scripts/check-dependency-direction.sh" \
  >"$mutant_log" 2>&1; then
  fail "self-test survived removal of --all-features from the wrapper"
fi
grep -Fq \
  "non-default feature forbidden edge unexpectedly passed" \
  "$mutant_log" ||
  fail "all-features mutant was rejected for the wrong reason"

unknown="$TEMP_ROOT/unknown-role"
write_workspace "$unknown" nmp-mystery
write_crate "$unknown" nmp-mystery
expect_failure \
  "unknown NMP role" \
  "$unknown" \
  "workspace package 'nmp-mystery' has no role classification"

# Synthetic Cargo metadata supplies two same-name intermediary packages with
# distinct exact package ids/sources. Every input-order permutation must emit
# byte-identical diagnostics and choose the lexicographically smallest complete
# minimum-edge path.
python3 - "$TEMP_ROOT" <<'PY'
import copy
import json
import os
import sys

root = sys.argv[1]
source = "path+file:///workspace/nmp-nip99#0.1.0"
target = "path+file:///workspace/nmp-store#0.1.0"
bridge_a = "git+https://example.invalid/bridge#bridge@0.1.0"
bridge_b = "registry+https://example.invalid/index#bridge@0.1.0"

def package(package_id, name, dependencies):
    return {
        "id": package_id,
        "name": name,
        "version": "0.1.0",
        "source": None if package_id.startswith("path+") else package_id.split("#", 1)[0],
        "dependencies": [{"name": value} for value in dependencies],
    }

def dependency(package_id):
    return {
        "name": "dependency",
        "pkg": package_id,
        "dep_kinds": [{"kind": None, "target": None}],
    }

base = {
    "packages": [
        package(source, "nmp-nip99", ["bridge-git", "bridge-registry"]),
        package(target, "nmp-store", []),
        package(bridge_a, "bridge", ["nmp-store"]),
        package(bridge_b, "bridge", ["nmp-store"]),
    ],
    "workspace_members": [source, target],
    "resolve": {
        "nodes": [
            {"id": source, "deps": [dependency(bridge_a), dependency(bridge_b)]},
            {"id": target, "deps": []},
            {"id": bridge_a, "deps": [dependency(target)]},
            {"id": bridge_b, "deps": [dependency(target)]},
        ]
    },
}

variants = []
variants.append(base)
members = copy.deepcopy(base)
members["workspace_members"].reverse()
variants.append(members)
packages = copy.deepcopy(base)
packages["packages"].reverse()
variants.append(packages)
nodes = copy.deepcopy(base)
nodes["resolve"]["nodes"].reverse()
variants.append(nodes)
declarations = copy.deepcopy(base)
declarations["packages"][0]["dependencies"].reverse()
declarations["resolve"]["nodes"][0]["deps"].reverse()
variants.append(declarations)
everything = copy.deepcopy(base)
everything["workspace_members"].reverse()
everything["packages"].reverse()
everything["resolve"]["nodes"].reverse()
for node in everything["resolve"]["nodes"]:
    node["deps"].reverse()
for item in everything["packages"]:
    item["dependencies"].reverse()
variants.append(everything)

for index, value in enumerate(variants):
    with open(
        os.path.join(root, "diamond-{}.json".format(index)),
        "w",
        encoding="utf-8",
    ) as target_file:
        json.dump(value, target_file)
PY

canonical_diagnostic=
for metadata in "$TEMP_ROOT"/diamond-*.json; do
  set +e
  diagnostic=$(python3 "$VALIDATOR" "$POLICY" "$metadata" 2>&1)
  diagnostic_status=$?
  set -e
  [[ $diagnostic_status -eq 1 ]] ||
    fail "permuted diamond metadata exited $diagnostic_status instead of 1"
  if [[ -z $canonical_diagnostic ]]; then
    canonical_diagnostic=$diagnostic
  elif [[ $diagnostic != "$canonical_diagnostic" ]]; then
    fail "permuted metadata changed dependency diagnostics"
  fi
done
grep -Fq \
  "shortest path: nmp-nip99 -> bridge [git+https://example.invalid/bridge#bridge@0.1.0] -> nmp-store" \
  <<<"$canonical_diagnostic" ||
  fail "diamond did not choose the canonical colliding-name path"

malformed_policy="$TEMP_ROOT/malformed-policy.json"
malformed_metadata="$TEMP_ROOT/malformed-metadata.json"
printf '{\n' >"$malformed_policy"
printf '{\n' >"$malformed_metadata"
expect_validator_failure \
  "malformed policy" \
  "$malformed_policy" \
  "$nip29_metadata" \
  "cannot read"
expect_validator_failure \
  "malformed metadata" \
  "$POLICY" \
  "$malformed_metadata" \
  "cannot read"

echo "dependency-direction test: all graph and wrapper falsifiers passed"
