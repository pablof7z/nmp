#!/usr/bin/env bash
# #1403: prepared Swift and Kotlin products must drive the same engine-owned
# cold-discovery path from exact app indexers, with independent relay witnesses.

set -euo pipefail

script_path=${BASH_SOURCE[0]}
script_dir=${script_path%/*}
[[ $script_dir != "$script_path" ]] || script_dir=.
# shellcheck disable=SC1091
source "$script_dir/lib/require-commands.sh" || exit 2
require_commands cargo cp git mkdir mktemp python3 rm seq sleep swift || exit 2

repo_root=$(git rev-parse --show-toplevel)
cd "$repo_root"
cargo build --locked -q -p nmp-cli --bin nmp
cargo build --locked -q -p nmp-test-support --bin outbox-routing-relay-harness
nmp_cli="${CARGO_TARGET_DIR:-$repo_root/target}/debug/nmp"
relay_harness="${CARGO_TARGET_DIR:-$repo_root/target}/debug/outbox-routing-relay-harness"
scratch_parent=${NMP_OUTBOX_ROUTING_SCRATCH_PARENT:-${RUNNER_TEMP:-${TMPDIR:-/tmp}}}
scratch=$(mktemp -d "$scratch_parent/nmp-outbox-routing.XXXXXX")
harness_pid=
cleanup() {
    if [[ -n $harness_pid ]] && kill -0 "$harness_pid" 2>/dev/null; then
        kill "$harness_pid" 2>/dev/null || true
        wait "$harness_pid" 2>/dev/null || true
    fi
    rm -rf "$scratch"
}
trap cleanup EXIT INT TERM

app="$scratch/app"
mkdir -p "$app"
cp -R fixtures/native-outbox-routing-runtime/. "$app/"
"$nmp_cli" --manifest "$app/.nmp.toml" prepare \
    --output "$app/Generated/NMP" \
    --cache-dir "$scratch/cache"
"$nmp_cli" verify --output "$app/Generated/NMP"

run_harness() {
    local platform=$1
    local manifest="$scratch/$platform-manifest.json"
    local stop="$scratch/$platform-stop"
    local report="$scratch/$platform-report.json"
    "$relay_harness" "$manifest" "$stop" "$report" &
    harness_pid=$!
    for _ in $(seq 1 400); do
        [[ -s $manifest ]] && break
        kill -0 "$harness_pid"
        sleep 0.025
    done
    [[ -s $manifest ]] || {
        echo "error: $platform relay harness did not publish its manifest" >&2
        return 1
    }

    if [[ $platform == swift ]]; then
        swift run --package-path "$app" OutboxRoutingSwiftConsumer "$manifest"
    else
        Packages/NMPKotlin/gradlew --no-daemon --console=plain \
            -p "$app/kotlin-consumer" run --args="$manifest"
    fi
    : > "$stop"
    wait "$harness_pid"
    harness_pid=
    python3 - "$report" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    report = json.load(handle)
assert report == {
    "passed": True,
    "indexer_kind_10002_queries": report["indexer_kind_10002_queries"],
    "author_scoped_kind_10002_queries": report["author_scoped_kind_10002_queries"],
    "indexer_events": 0,
    "outbox_events": 1,
    "undeclared_contacts": 0,
}
assert report["indexer_kind_10002_queries"] > 0
assert report["author_scoped_kind_10002_queries"] > 0
print(json.dumps(report, sort_keys=True))
PY
}

run_harness swift
run_harness kotlin

echo "check-native-outbox-routing-runtime: prepared Swift and Kotlin cold discovery passed"
