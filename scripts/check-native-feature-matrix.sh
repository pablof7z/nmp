#!/usr/bin/env bash
set -euo pipefail

SCRIPT_PATH=${BASH_SOURCE[0]}
SCRIPT_DIR=${SCRIPT_PATH%/*}
[[ $SCRIPT_DIR != "$SCRIPT_PATH" ]] || SCRIPT_DIR=.
source "$SCRIPT_DIR/lib/require-commands.sh" || exit 2
require_commands cargo git || exit 2

root=$(git rev-parse --show-toplevel)
cd "$root"

cargo build --locked -q --release -p nmp-cli
nmp_cli="${CARGO_TARGET_DIR:-target}/release/nmp"
"$nmp_cli" --help >/dev/null

features=()
while IFS= read -r feature; do
  features+=("$feature")
done < <("$nmp_cli" capability list | awk '{print $2}')

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

# The app-facing runtime contract belongs to the selected outbox-routing build, so its
# proofs run with that exact optional surface rather than pretending a
# feature-disabled default-build test is executable behavioral evidence.
for proof in \
  facade::tests::selected_outbox_routing_refuses_an_empty_runtime_indexer_set \
  facade::tests::providerless_auto_refuses_before_acceptance_and_leaves_no_residue \
  facade::tests::selected_outbox_routing_discovers_and_publishes_to_the_cold_outbox
do
  echo "native-feature-matrix: running $proof"
  cargo test --locked -p nmp-ffi --no-default-features --features nip65 \
    "$proof" -- --exact
done

cargo test --locked -p nmp-cli
