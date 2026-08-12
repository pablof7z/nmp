#!/usr/bin/env bash
# Build, inspect, and externally consume one feature-selected Android AAR.

set -euo pipefail

script_path=${BASH_SOURCE[0]}
script_dir=${script_path%/*}
[[ $script_dir != "$script_path" ]] || script_dir=.
# shellcheck disable=SC1091
source "$script_dir/lib/require-commands.sh" || exit 2
require_commands git || exit 2

repo_root=$(git rev-parse --show-toplevel)
cd "$repo_root"
cargo build --locked -q --release -p nmp-cli
nmp_cli="${CARGO_TARGET_DIR:-$repo_root/target}/release/nmp"
manifest=${1:?usage: check-android-aar.sh <manifest> <output> <cache>}
output=${2:?usage: check-android-aar.sh <manifest> <output> <cache>}
cache=${3:?usage: check-android-aar.sh <manifest> <output> <cache>}
consumer="$repo_root/fixtures/android-aar-consumer"
gradle="$repo_root/Packages/NMPKotlin/gradlew"

"$nmp_cli" prepare \
    --manifest "$manifest" \
    --output "$output" \
    --cache-dir "$cache"

"$repo_root/scripts/verify-android-aar.py" verify \
    --repo "$repo_root" \
    --output "$output"

if git -C "$repo_root" grep -n -E \
    'uniffi\.nmp_ffi|repository-relative|System\.load|Native\.load' -- \
    fixtures/android-aar-consumer/app/src/main/kotlin; then
    echo "error: Android qualification consumer bypasses com.nmp.sdk" >&2
    exit 1
fi

"$gradle" \
    --no-daemon \
    --console=plain \
    -p "$consumer" \
    -PnmpAndroidRepository="$output/android/repository" \
    :app:assembleDebug

echo "check-android-aar: selected AAR verified and consumed by a clean app"
