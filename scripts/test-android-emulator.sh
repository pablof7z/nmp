#!/usr/bin/env bash
# Run #832's external Maven consumer on an already-booted API-35 emulator.

set -euo pipefail

script_path=${BASH_SOURCE[0]}
script_dir=${script_path%/*}
[[ $script_dir != "$script_path" ]] || script_dir=.
# shellcheck disable=SC1091
source "$script_dir/lib/require-commands.sh" || exit 2
require_commands adb awk cp find git grep head mkdir mkfifo mktemp sed shasum timeout tr unzip zip || exit 2

repo_root=$(git rev-parse --show-toplevel)
runtime_output=${NMP_ANDROID_RUNTIME_OUTPUT:?NMP_ANDROID_RUNTIME_OUTPUT must name #831 output}
aar="$runtime_output/android/artifacts/nmp-android-0.0.0.aar"
repository="$runtime_output/android/repository"
active_repository="$repository"
provenance="$runtime_output/nmp-native-provenance.json"
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

rm -rf "$artifacts"
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
[[ -f "$provenance" ]] || { echo "error: prepared-product provenance missing: $provenance" >&2; exit 1; }
[[ -x "$relay_bin" ]] || { echo "error: controlled relay missing: $relay_bin" >&2; exit 1; }

if git -C "$repo_root" grep -n -E \
    'uniffi\.|System\.(load|loadLibrary)|Native\.load|implementation\((files|project)|api\(project' -- \
    fixtures/android-aar-consumer/app/src fixtures/android-aar-consumer/app/build.gradle.kts; then
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
cp "$repo_root/fixtures/android-aar-consumer/.nmp.toml" "$artifacts/core-product.nmp.toml"
cp "$provenance" "$artifacts/nmp-native-provenance.json"

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
        -PnmpAndroidRepository="$active_repository" \
        -PnmpQualificationRelay="$success_url" \
        -PnmpQualificationRecoveryRelay="$recovery_url" \
        -PnmpQualificationOfflineRelay="$offline_url" \
        "$@"
}

install_consumer() {
    local app_apk="$consumer/app/build/outputs/apk/release/app-release.apk"
    local test_apk="$consumer/app/build/outputs/apk/androidTest/release/app-release-androidTest.apk"
    adb install -r "$app_apk"
    adb install -r "$test_apk"
}

start_relay success "$success_port" 0
success_relay_pid=$started_relay_pid
start_relay recovery "$recovery_port" 2

assemble_consumer :app:clean :app:assembleRelease :app:assembleReleaseAndroidTest
assemble_consumer :app:dependencies --configuration releaseRuntimeClasspath \
    > "$artifacts/resolved-dependencies.txt"
grep -q 'com.nmp:nmp-android:0.0.0' "$artifacts/resolved-dependencies.txt"
install_consumer
app_apk="$consumer/app/build/outputs/apk/release/app-release.apk"
test_apk="$consumer/app/build/outputs/apk/androidTest/release/app-release-androidTest.apk"
cp "$app_apk" \
    "$artifacts/nmp-runtime-qualification-release.apk"
cp "$test_apk" \
    "$artifacts/nmp-runtime-qualification-androidTest.apk"
aar_native_sha=$(unzip -p "$aar" jni/x86_64/libnmp_ffi.so | shasum -a 256 | awk '{print $1}')
apk_native_sha=$(unzip -p "$app_apk" lib/x86_64/libnmp_ffi.so | shasum -a 256 | awk '{print $1}')
[[ -n "$aar_native_sha" && "$aar_native_sha" == "$apk_native_sha" ]] || {
    echo "error: APK native payload does not match the prepared core AAR" >&2
    exit 1
}
{
    echo "manifest_sha256=$(shasum -a 256 "$repo_root/fixtures/android-aar-consumer/.nmp.toml" | awk '{print $1}')"
    echo "prepared_provenance_sha256=$(shasum -a 256 "$provenance" | awk '{print $1}')"
    echo "aar_sha256=$(shasum -a 256 "$aar" | awk '{print $1}')"
    echo "aar_x86_64_native_sha256=$aar_native_sha"
    echo "apk_sha256=$(shasum -a 256 "$app_apk" | awk '{print $1}')"
    echo "apk_x86_64_native_sha256=$apk_native_sha"
    echo "android_test_apk_sha256=$(shasum -a 256 "$test_apk" | awk '{print $1}')"
} > "$artifacts/exact-product-provenance.txt"
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

missing_version=0.0.0-missing-x86_64
missing_repository="$scratch/missing-abi-repository"
source_version_dir="$repository/com/nmp/nmp-android/0.0.0"
missing_version_dir="$missing_repository/com/nmp/nmp-android/$missing_version"
missing_abi_aar="$missing_version_dir/nmp-android-$missing_version.aar"
missing_abi_pom="$missing_version_dir/nmp-android-$missing_version.pom"
mkdir -p "$missing_version_dir"
cp "$source_version_dir/nmp-android-0.0.0.aar" "$missing_abi_aar"
sed "s/0\\.0\\.0/$missing_version/g" \
    "$source_version_dir/nmp-android-0.0.0.pom" > "$missing_abi_pom"
zip -dq "$missing_abi_aar" 'jni/x86_64/libnmp_ffi.so'
unzip -l "$missing_abi_aar" > "$artifacts/missing-abi-aar-inventory.txt"
shasum -a 256 "$missing_abi_aar" > "$artifacts/missing-abi-aar-sha256.txt"
cp "$missing_abi_pom" "$artifacts/missing-abi-pom.xml"
adb uninstall "$package" >/dev/null || true
active_repository="$missing_repository"
assemble_consumer \
    -PnmpQualificationCoordinate="com.nmp:nmp-android:$missing_version" \
    -PnmpExpectNativeLoad=false \
    :app:clean :app:assembleRelease :app:assembleReleaseAndroidTest
install_consumer
run_test missingEmulatorAbiFailsAtNativeConstruction "$artifacts/missing-abi.txt"
adb logcat -d -s NMPQualification:I '*:S' >> "$artifacts/qualification-logcat.txt"
grep -q NMP_ANDROID_WRONG_ABI_REFUSED "$artifacts/qualification-logcat.txt"

echo "test-android-emulator: public facade, persistence, failure, performance, and ABI controls passed"
