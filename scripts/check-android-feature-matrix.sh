#!/usr/bin/env bash
# Issue #831 Android platform rows and adversarial packaging controls.

set -euo pipefail

script_path=${BASH_SOURCE[0]}
script_dir=${script_path%/*}
[[ $script_dir != "$script_path" ]] || script_dir=.
# shellcheck disable=SC1091
source "$script_dir/lib/require-commands.sh" || exit 2
require_commands cp git head mkdir mktemp rm sed zip || exit 2

repo_root=$(git rev-parse --show-toplevel)
cd "$repo_root"
cargo build --locked -q --release -p nmp-cli
nmp_cli="${CARGO_TARGET_DIR:-$repo_root/target}/release/nmp"
scratch_parent=${NMP_ANDROID_SCRATCH_PARENT:-${RUNNER_TEMP:-${TMPDIR:-/tmp}}}
mkdir -p "$scratch_parent"
scratch=$(mktemp -d "$scratch_parent/nmp-android-matrix.XXXXXX")
trap 'rm -rf "$scratch"' EXIT
mkdir -p "$scratch/tmp"
export TMPDIR="$scratch/tmp"
cache="$scratch/cache"

prepare() {
    local row_name=$1
    local manifest_path=$2
    shift 2
    "$nmp_cli" prepare \
        --manifest "$manifest_path" \
        "$@" \
        --output "$scratch/$row_name" \
        --cache-dir "$cache"
    "$repo_root/scripts/verify-android-aar.py" verify \
        --repo "$repo_root" \
        --output "$scratch/$row_name"
}

prepare core "$repo_root/native/examples/android-core.toml"
prepare normal "$repo_root/native/examples/android-normal-client.toml"
prepare all "$repo_root/native/examples/android-all.toml"
prepare mix "$repo_root/native/examples/android-representative-mix.toml"

"$repo_root/scripts/verify-android-aar.py" parity \
    --repo "$repo_root" \
    --output "$scratch/normal"

gradle="$repo_root/Packages/NMPKotlin/gradlew"
consumer="$repo_root/fixtures/android-aar-consumer"
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
    -PnmpAndroidRepository="$scratch/normal/android/repository" \
    :app:assembleDebug

if "$gradle" \
    --no-daemon \
    --console=plain \
    -p "$consumer" \
    -PnmpAndroidRepository="$scratch/core/android/repository" \
    -PnmpCompileUnselectedControl=true \
    :app:compileDebugKotlin; then
    echo "error: unselected Android facade control unexpectedly compiled" >&2
    exit 1
fi

normal_aar="$scratch/normal/android/artifacts/nmp-android-0.0.0.aar"
incomplete_aar="$scratch/incomplete.aar"
cp "$normal_aar" "$incomplete_aar"
zip -dq "$incomplete_aar" 'jni/x86_64/libnmp_ffi.so'
if "$repo_root/scripts/verify-android-aar.py" verify \
    --repo "$repo_root" \
    --output "$scratch/normal" \
    --aar "$incomplete_aar"; then
    echo "error: missing-ABI control unexpectedly passed" >&2
    exit 1
fi

binding="$scratch/normal/android/src/main/kotlin/uniffi/nmp_ffi/nmp_ffi.kt"
mismatched_binding="$scratch/mismatched.kt"
checksum=$(sed -nE 's/^[[:space:]]*fun[[:space:]]+([A-Za-z0-9_]*checksum_[A-Za-z0-9_]*).*/\1/p' "$binding" | head -n 1)
if [[ -z "$checksum" ]]; then
    echo "error: generated Android binding has no checksum symbol" >&2
    exit 1
fi
sed "s/$checksum/${checksum}_mismatched/" "$binding" > "$mismatched_binding"
if "$repo_root/scripts/verify-android-aar.py" verify \
    --repo "$repo_root" \
    --output "$scratch/normal" \
    --binding "$mismatched_binding"; then
    echo "error: binding/native mismatch control unexpectedly passed" >&2
    exit 1
fi

second_cache="$scratch/second-cache"
rm -rf "$cache"
"$nmp_cli" prepare \
    --manifest "$repo_root/native/examples/android-normal-client.toml" \
    --output "$scratch/normal-second" \
    --cache-dir "$second_cache"
"$repo_root/scripts/verify-android-aar.py" compare \
    "$normal_aar" \
    "$scratch/normal-second/android/artifacts/nmp-android-0.0.0.aar"

if [[ -n "${NMP_ANDROID_HOSTED_DIR:-}" ]]; then
    mkdir -p "$NMP_ANDROID_HOSTED_DIR"
    cp "$normal_aar" "$NMP_ANDROID_HOSTED_DIR/nmp-android.aar"
    cp "$scratch/normal/nmp-native-provenance.json" \
        "$NMP_ANDROID_HOSTED_DIR/nmp-native-provenance.json"
    cp "$scratch/normal/android/nmp-native-selection.json" \
        "$NMP_ANDROID_HOSTED_DIR/nmp-native-selection.json"
    "$repo_root/scripts/verify-android-aar.py" verify \
        --repo "$repo_root" \
        --output "$scratch/normal" \
        > "$NMP_ANDROID_HOSTED_DIR/verification.json"
fi

echo "check-android-feature-matrix: all platform rows and controls passed"
