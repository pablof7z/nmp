#!/usr/bin/env bash
# #1392: one clean .nmp.toml prepares both native products; ordinary consumer
# builds must not invoke Cargo, Python, or source-copy machinery.

set -euo pipefail

script_path=${BASH_SOURCE[0]}
script_dir=${script_path%/*}
[[ $script_dir != "$script_path" ]] || script_dir=.
# shellcheck disable=SC1091
source "$script_dir/lib/require-commands.sh" || exit 2
require_commands cargo cp git mkdir mktemp python3 rm xcrun || exit 2

repo_root=$(git rev-parse --show-toplevel)
cd "$repo_root"
cargo build --locked -q --release -p nmp-cli
nmp_cli="${CARGO_TARGET_DIR:-$repo_root/target}/release/nmp"
scratch_parent=${NMP_NATIVE_CLI_SCRATCH_PARENT:-${RUNNER_TEMP:-${TMPDIR:-/tmp}}}
scratch=$(mktemp -d "$scratch_parent/nmp-native-cli.XXXXXX")
trap 'rm -rf "$scratch"' EXIT
app="$scratch/app"
mkdir -p "$app/Sources"
cp fixtures/native-cli-app/.nmp.toml fixtures/native-cli-app/Package.swift "$app/"
cp -R fixtures/native-cli-app/Sources/NMPNativeCLIConsumer "$app/Sources/"

"$nmp_cli" --manifest "$app/.nmp.toml" prepare \
    --output "$app/Generated/NMP" \
    --cache-dir "$scratch/cache"
"$nmp_cli" verify --output "$app/Generated/NMP"
scripts/verify-android-aar.py verify \
    --repo "$repo_root" \
    --output "$app/Generated/NMP"

witness="$scratch/process-witness"
mkdir -p "$witness/bin"
for command_name in cargo python python3; do
    printf '#!/bin/sh\nprintf "%%s\\n" %s >> %s\nexit 97\n' \
        "$command_name" "$witness/invocations" > "$witness/bin/$command_name"
    chmod +x "$witness/bin/$command_name"
done
original_path=$PATH
PATH="$witness/bin:$PATH" xcrun swift build --package-path "$app"

gradle="$repo_root/Packages/NMPKotlin/gradlew"
consumer="$repo_root/fixtures/android-aar-consumer"
PATH="$witness/bin:$original_path" "$gradle" \
    --no-daemon \
    --console=plain \
    -p "$consumer" \
    -PnmpAndroidRepository="$app/Generated/NMP/android/repository" \
    :app:assembleDebug

if [[ -s "$witness/invocations" ]]; then
    echo "error: ordinary native consumer build invoked a forbidden preparation tool" >&2
    cat "$witness/invocations" >&2
    exit 1
fi

echo "check-native-cli-consumers: clean Swift and Android consumers used only the prepared product"
