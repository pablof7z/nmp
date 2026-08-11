#!/usr/bin/env bash
set -euo pipefail

SCRIPT_PATH=${BASH_SOURCE[0]}
SCRIPT_DIR=${SCRIPT_PATH%/*}
[[ $SCRIPT_DIR != "$SCRIPT_PATH" ]] || SCRIPT_DIR=.
source "$SCRIPT_DIR/lib/require-commands.sh" || exit 2
require_commands cargo git python3 || exit 2

root=$(git rev-parse --show-toplevel)
cd "$root"

scripts/nmp-native --help >/dev/null

features=()
while IFS= read -r feature; do
  features+=("$feature")
done < <(
  python3 - <<'PY'
import pathlib
import tomllib

catalog = tomllib.loads(pathlib.Path("native/features.toml").read_text())
for record in catalog["features"]:
    print(record["cargo_feature"])
PY
)

if ((${#features[@]} == 0)); then
  echo "native-feature-matrix: catalog contains no app-facing features" >&2
  exit 1
fi

echo "native-feature-matrix: checking core-only"
cargo check --locked -p nmp-ffi --no-default-features

for feature in "${features[@]}"; do
  echo "native-feature-matrix: checking $feature"
  cargo check --locked -p nmp-ffi --no-default-features --features "$feature"
done

echo "native-feature-matrix: checking all features"
cargo check --locked -p nmp-ffi --all-features

python3 tools/nmp-native/test_nmp_native.py
