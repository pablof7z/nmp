#!/usr/bin/env bash
# #851/#1239/#824: nmp-ffi consumes the canonical nmp facade and exposes one
# catalog-selected projection. Mechanism/protocol crates stay behind facade
# forwarding features; NIP-02 is the sole direct optional exception because it
# depends on nmp and therefore cannot sit below the facade without a cycle.
set -euo pipefail

SCRIPT_PATH=${BASH_SOURCE[0]}
SCRIPT_DIR=${SCRIPT_PATH%/*}
[[ $SCRIPT_DIR != "$SCRIPT_PATH" ]] || SCRIPT_DIR=.
source "$SCRIPT_DIR/lib/require-commands.sh" || exit 2
require_commands git grep python3 xargs || exit 2

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"

fail() { echo "ffi-facade-boundary: $*" >&2; exit 1; }

FFI_MANIFEST=crates/nmp-ffi/Cargo.toml
FACADE_MANIFEST=crates/nmp/Cargo.toml
CATALOG=native/features.toml

for required in "$FFI_MANIFEST" "$FACADE_MANIFEST" "$CATALOG" crates/nmp-ffi/src; do
  [[ -e $required ]] || fail "required path is missing: $required"
done

# The catalog is the only family inventory. For every catalog record, require
# one nmp-ffi forwarding feature and validate every activation as either a real
# nmp facade feature, another catalog forwarding feature (a Cargo dependency
# edge), or the one cycle-breaking optional NIP-02 dependency. The canonical
# nmp dependency itself must carry no fixed feature bundle.
python3 - "$FFI_MANIFEST" "$FACADE_MANIFEST" "$CATALOG" <<'PY'
from __future__ import annotations

import sys
import tomllib
from pathlib import Path


def fail(message: str) -> None:
    print(f"ffi-facade-boundary: {message}", file=sys.stderr)
    raise SystemExit(1)


ffi_path, facade_path, catalog_path = map(Path, sys.argv[1:])
ffi = tomllib.loads(ffi_path.read_text(encoding="utf-8"))
facade = tomllib.loads(facade_path.read_text(encoding="utf-8"))
catalog = tomllib.loads(catalog_path.read_text(encoding="utf-8"))

if catalog.get("schema") != 3:
    fail("native feature catalog schema must be 3")
records = catalog.get("features")
if not isinstance(records, list) or not records:
    fail("native feature catalog must contain app-facing records")

ffi_features = ffi.get("features")
facade_features = facade.get("features")
if not isinstance(ffi_features, dict) or not isinstance(facade_features, dict):
    fail("both nmp-ffi and nmp must declare feature tables")
if ffi_features.get("default") != []:
    fail("nmp-ffi default features must be exactly empty")

cargo_features: list[str] = []
for index, record in enumerate(records):
    if not isinstance(record, dict):
        fail(f"catalog feature record {index} is not a table")
    feature = record.get("cargo_feature")
    if not isinstance(feature, str) or not feature:
        fail(f"catalog feature record {index} lacks cargo_feature")
    cargo_features.append(feature)
if len(cargo_features) != len(set(cargo_features)):
    fail("catalog cargo_feature values must be unique")

catalog_feature_set = set(cargo_features)
for feature in cargo_features:
    activations = ffi_features.get(feature)
    if not isinstance(activations, list) or not activations:
        fail(f"nmp-ffi feature {feature} is missing or activates nothing")
    for activation in activations:
        if not isinstance(activation, str):
            fail(f"nmp-ffi feature {feature} has a non-string activation")
        if activation.startswith("nmp/"):
            facade_feature = activation.removeprefix("nmp/")
            if facade_feature not in facade_features:
                fail(
                    f"nmp-ffi feature {feature} forwards missing facade feature "
                    f"nmp/{facade_feature}"
                )
        elif activation == "dep:nmp-nip02":
            if feature != "nip02":
                fail("only the nip02 forwarding feature may activate dep:nmp-nip02")
        elif activation not in catalog_feature_set:
            fail(
                f"nmp-ffi feature {feature} has unregistered activation {activation}"
            )

dependencies = ffi.get("dependencies")
if not isinstance(dependencies, dict):
    fail("nmp-ffi has no dependencies table")
nmp_dependency = dependencies.get("nmp")
if not isinstance(nmp_dependency, dict):
    fail("nmp-ffi is missing its canonical nmp dependency")
if "features" in nmp_dependency:
    fail("nmp-ffi must not hard-code a feature bundle on its nmp dependency")

for dependency, declaration in dependencies.items():
    if dependency == "nmp":
        continue
    if dependency.startswith("nmp-") and dependency != "nmp-nip02":
        fail(f"nmp-ffi has a forbidden direct normal dependency: {dependency}")
if dependencies.get("nmp-nip02", {}).get("optional") is not True:
    fail("the cycle-breaking nmp-nip02 dependency must remain optional")
PY

# Dev-only residual mechanism vocabulary remains a shrinking exact allowance.
# Production cannot bind these crates because the manifest check above rejects
# the normal dependency edge; this census prevents test code from turning that
# narrow allowance into an informal second surface.
census() {
  git ls-files -- 'crates/nmp-ffi/*.rs' | xargs grep -nE "$1" 2>/dev/null || true
}
strip_comments() { grep -vE '^[^:]*:[0-9]+:[[:space:]]*//' || true; }

ALLOWED_RESIDUAL='nmp_grammar::ConcreteFilter'
found=$(census '\bnmp_(grammar|signer)::' |
  strip_comments |
  grep -vE "$ALLOWED_RESIDUAL" || true)
if [[ -n $found ]]; then
  printf '%s\n' "$found"
  fail "nmp-ffi imports a mechanism value the nmp facade already projects"
fi

census '\bnmp_grammar::ConcreteFilter\b' | strip_comments | grep -q . ||
  fail "residual allowance is stale and must be removed: nmp_grammar::ConcreteFilter"

echo "ffi-facade-boundary: ok"
