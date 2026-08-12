#!/usr/bin/env bash
# Run #832's external Maven consumer on an already-booted API-35 emulator.

set -euo pipefail

script_path=${BASH_SOURCE[0]}
script_dir=${script_path%/*}
[[ $script_dir != "$script_path" ]] || script_dir=.
# shellcheck disable=SC1091
source "$script_dir/lib/require-commands.sh" || exit 2
require_commands adb cp git grep head mkdir mkfifo mktemp sed timeout unzip zip || exit 2

repo_root=$(git rev-parse --show-toplevel)
runtime_output=${NMP_ANDROID_RUNTIME_OUTPUT:?NMP_ANDROID_RUNTIME_OUTPUT must name #831 output}
aar="$runtime_output/android/artifacts/nmp-android-0.0.0.aar"
repository="$runtime_output/android/repository"
relay_bin="$repo_root/target/release/android_controlled_relay"
gradle="$repo_root/Packages/NMPKotlin/gradlew"
artifacts="$repo_root/artifacts/android-emulator"
scratch_parent=${RUNNER_TEMP:-${TMPDIR:-/tmp}}
scratch=$(mktemp -d "$scratch_parent/nmp-android-runtime.XXXXXX")
consumer="$scratch/external-consumer"
package=com.nmp.qualification.consumer
runner="$package.test/androidx.test.runner.AndroidJUnitRunner"
success_port=47391
recovery_port=47392
offline_port=47393
success_url="ws://10.0.2.2:$success_port"
recovery_url="ws://10.0.2.2:$recovery_port"
offline_url="ws://10.0.2.2:$offline_port"
relay_pids=()
started_relay_pid=

mkdir -p "$artifacts"
cp -R "$repo_root/fixtures/android-aar-consumer" "$consumer"

capture_evidence() {
    adb logcat -d > "$artifacts/logcat.txt" 2>&1 || true
    adb shell getprop > "$artifacts/emulator-properties.txt" 2>&1 || true
    find "$consumer/app/build/outputs" -type f -print \
        > "$artifacts/consumer-output-inventory.txt" 2>&1 || true
    while IFS= read -r apk; do
        unzip -l "$apk" >> "$artifacts/apk-inventory.txt" 2>&1 || true
        cp "$apk" "$artifacts/" 2>/dev/null || true
    done < <(find "$consumer/app/build/outputs" -type f -name '*.apk' -print 2>/dev/null)
    for relay_pid in "${relay_pids[@]}"; do
        kill "$relay_pid" 2>/dev/null || true
        wait "$relay_pid" 2>/dev/null || true
    done
    rm -rf "$scratch"
}
trap capture_evidence EXIT

[[ -f "$aar" ]] || { echo "error: runtime AAR missing: $aar" >&2; exit 1; }
[[ -x "$relay_bin" ]] || { echo "error: controlled relay missing: $relay_bin" >&2; exit 1; }

if git -C "$repo_root" grep -n -E \
    'uniffi\.nmp_ffi|System\.load|Native\.load|repository-relative' -- \
    fixtures/android-aar-consumer/app/src/main; then
    echo "error: Android runtime consumer bypasses com.nmp.sdk" >&2
    exit 1
fi

{
    echo "source_commit=$(git -C "$repo_root" rev-parse HEAD)"
    echo "java=$("$JAVA_HOME"/bin/java -version 2>&1 | head -n 1)"
    echo "gradle=$($gradle --version | sed -n 's/^Gradle /Gradle /p' | head -n 1)"
    echo "adb=$(adb version | head -n 1)"
    echo "device=$(adb shell getprop ro.product.name | tr -d '\r')"
    echo "sdk=$(adb shell getprop ro.build.version.sdk | tr -d '\r')"
    echo "abi=$(adb shell getprop ro.product.cpu.abi | tr -d '\r')"
    echo "success_relay=$success_url"
    echo "recovery_relay=$recovery_url"
    echo "offline_relay=$offline_url"
} > "$artifacts/runtime-context.txt"
unzip -l "$aar" > "$artifacts/aar-inventory.txt"
shasum -a 256 "$aar" > "$artifacts/aar-sha256.txt"

start_relay() {
    local name=$1
    local port=$2
    local failures=$3
    local fifo="$scratch/$name.ready"
    local log="$artifacts/$name-relay.log"
    mkfifo "$fifo"
    timeout 15 grep -qx ready "$fifo" &
    local waiter=$!
    NMP_ANDROID_RELAY_PORT=$port \
        NMP_ANDROID_RELAY_FAIL_HANDSHAKES=$failures \
        NMP_ANDROID_RELAY_READY_FIFO=$fifo \
        "$relay_bin" > "$log" 2>&1 &
    local relay_pid=$!
    relay_pids+=("$relay_pid")
    started_relay_pid=$relay_pid
    wait "$waiter"
    grep -q NMP_ANDROID_RELAY_READY "$log"
}

run_test() {
    local method=$1
    local output=$2
    timeout 60 adb shell am instrument -w -r \
        -e class "$package.NMPRuntimeQualificationTest#$method" \
        "$runner" | tee "$output"
    grep -Eq 'OK \([1-9][0-9]* test' "$output"
    if grep -q 'FAILURES!!!' "$output"; then
        echo "error: instrumentation failed: $method" >&2
        exit 1
    fi
}

assemble_consumer() {
    "$gradle" \
        --no-daemon \
        --console=plain \
        -p "$consumer" \
        -PnmpAndroidRepository="$repository" \
        -PnmpQualificationRelay="$success_url" \
        -PnmpQualificationRecoveryRelay="$recovery_url" \
        -PnmpQualificationOfflineRelay="$offline_url" \
        "$@"
}

install_consumer() {
    local app_apk="$consumer/app/build/outputs/apk/debug/app-debug.apk"
    local test_apk="$consumer/app/build/outputs/apk/androidTest/debug/app-debug-androidTest.apk"
    adb install -r "$app_apk"
    adb install -r "$test_apk"
}

start_relay success "$success_port" 0
success_relay_pid=$started_relay_pid
start_relay recovery "$recovery_port" 2

assemble_consumer :app:clean :app:assembleDebug :app:assembleDebugAndroidTest
install_consumer
cp "$consumer/app/build/outputs/apk/debug/app-debug.apk" \
    "$artifacts/nmp-runtime-qualification-debug.apk"
cp "$consumer/app/build/outputs/apk/androidTest/debug/app-debug-androidTest.apk" \
    "$artifacts/nmp-runtime-qualification-androidTest.apk"
adb logcat -c
adb shell pm clear "$package" >/dev/null

run_test coldSeedUsesPublicFacadeAndAndroidStorage "$artifacts/cold-seed.txt"
grep -q NMP_ANDROID_RELAY_REQ "$artifacts/success-relay.log"
grep -q NMP_ANDROID_RELAY_CLOSE "$artifacts/success-relay.log"
kill "$success_relay_pid"
wait "$success_relay_pid" 2>/dev/null || true
for index in "${!relay_pids[@]}"; do
    [[ ${relay_pids[$index]} != "$success_relay_pid" ]] || unset 'relay_pids[index]'
done
adb shell am force-stop "$package"
run_test freshProcessReopensCacheAndMeetsCacheLatency "$artifacts/fresh-process-reopen.txt"
run_test preconnectFailureRecoversToRealRow "$artifacts/preconnect-recovery.txt"
grep -q 'NMP_ANDROID_RELAY_REFUSED attempt=1' "$artifacts/recovery-relay.log"
grep -q NMP_ANDROID_RELAY_REQ "$artifacts/recovery-relay.log"
run_test offlineFailureIsScopedAndJavaReadable "$artifacts/offline-evidence.txt"
run_test cancellationBeforeRequiredRowIsBounded "$artifacts/cancel-before-row.txt"
run_test collectorIdleAndTeardownPerformanceContract "$artifacts/performance-contract.txt"

adb logcat -d -s NMPQualification:I '*:S' > "$artifacts/qualification-logcat.txt"
for marker in \
    NMP_ANDROID_ACTIVITY_LAUNCHED \
    NMP_ANDROID_COLD_SEED \
    NMP_ANDROID_REOPENED \
    NMP_ANDROID_RECOVERED \
    NMP_ANDROID_OFFLINE \
    NMP_ANDROID_CANCELLED \
    NMP_ANDROID_PERFORMANCE; do
    grep -q "$marker" "$artifacts/qualification-logcat.txt"
done

seed_pid=$(sed -n 's/.*NMP_ANDROID_COLD_SEED pid=\([0-9][0-9]*\).*/\1/p' \
    "$artifacts/qualification-logcat.txt" | head -n 1)
reopen_pid=$(sed -n 's/.*NMP_ANDROID_REOPENED pid=\([0-9][0-9]*\).*/\1/p' \
    "$artifacts/qualification-logcat.txt" | head -n 1)
[[ -n "$seed_pid" && -n "$reopen_pid" && "$seed_pid" != "$reopen_pid" ]] || {
    echo "error: store reopen was not proven in a fresh process" >&2
    exit 1
}

missing_abi_aar="$artifacts/nmp-android-missing-x86_64.aar"
cp "$aar" "$missing_abi_aar"
zip -dq "$missing_abi_aar" 'jni/x86_64/libnmp_ffi.so'
unzip -l "$missing_abi_aar" > "$artifacts/missing-abi-aar-inventory.txt"
adb uninstall "$package" >/dev/null || true
assemble_consumer \
    -PnmpMissingRuntimeAar="$missing_abi_aar" \
    :app:clean :app:assembleDebug :app:assembleDebugAndroidTest
install_consumer
run_test missingEmulatorAbiFailsAtNativeConstruction "$artifacts/missing-abi.txt"
adb logcat -d -s NMPQualification:I '*:S' >> "$artifacts/qualification-logcat.txt"
grep -q NMP_ANDROID_WRONG_ABI_REFUSED "$artifacts/qualification-logcat.txt"

echo "test-android-emulator: public facade, persistence, failure, performance, and ABI controls passed"
