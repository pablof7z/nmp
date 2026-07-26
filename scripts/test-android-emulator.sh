#!/usr/bin/env bash
# Run #832/#833/#834's external AAR consumer on the governed emulator.
#
# reactivecircus/android-emulator-runner owns emulator creation and calls this
# script only after boot completion. The host relay stays outside the app;
# Android reaches it through the documented 10.0.2.2 host-loopback alias.

set -euo pipefail

REPO_ROOT=$(git rev-parse --show-toplevel)
cd "$REPO_ROOT"

ARTIFACTS="$REPO_ROOT/artifacts/android-emulator"
ANDROID_PROJECT="$REPO_ROOT/Packages/NMPAndroid"
CONSUMER="$REPO_ROOT/fixtures/android-aar-consumer"
QUALIFICATION_REPOSITORY="$ANDROID_PROJECT/build/qualification-repository"
RELAY_PORT=47391
RELAY_URL="ws://10.0.2.2:$RELAY_PORT"
NIP46_RELAY_URL="ws://127.0.0.1:$RELAY_PORT"
RELAY_LOG="$ARTIFACTS/controlled-relay.log"
GRADLE="$REPO_ROOT/Packages/NMPKotlin/gradlew"
AAR="$ANDROID_PROJECT/build/outputs/aar/NMPAndroid-release.aar"
MISSING_ABI_AAR="$ARTIFACTS/NMPAndroid-missing-x86_64.aar"

mkdir -p "$ARTIFACTS"

relay_pid=
nip46_pairing_secret=
capture_runtime_evidence() {
    adb logcat -d > "$ARTIFACTS/logcat.txt" 2>&1 || true
    adb shell getprop > "$ARTIFACTS/emulator-properties.txt" 2>&1 || true
    adb reverse --list > "$ARTIFACTS/adb-reverse.txt" 2>&1 || true
    find "$CONSUMER/app/build/outputs" -type f -print \
        > "$ARTIFACTS/consumer-output-inventory.txt" 2>&1 || true
    while IFS= read -r apk; do
        unzip -l "$apk" >> "$ARTIFACTS/apk-inventory.txt" 2>&1 || true
    done < <(find "$CONSUMER/app/build/outputs" -type f -name '*.apk' -print 2>/dev/null)
    if [[ -n "$relay_pid" ]]; then
        kill "$relay_pid" 2>/dev/null || true
        wait "$relay_pid" 2>/dev/null || true
    fi
    adb reverse --remove "tcp:$RELAY_PORT" 2>/dev/null || true
}
trap capture_runtime_evidence EXIT

{
    echo "java=$("$JAVA_HOME/bin/java" -version 2>&1 | head -n 1)"
    echo "adb=$(adb version | head -n 1)"
    echo "device=$(adb shell getprop ro.product.name | tr -d '\r')"
    echo "sdk=$(adb shell getprop ro.build.version.sdk | tr -d '\r')"
    echo "abi=$(adb shell getprop ro.product.cpu.abi | tr -d '\r')"
    echo "relay=$RELAY_URL"
    echo "nip46_relay=$NIP46_RELAY_URL"
} > "$ARTIFACTS/runtime-context.txt"
unzip -l "$AAR" > "$ARTIFACTS/aar-inventory.txt"

nip46_pairing_secret=$(openssl rand -hex 32)
NMP_ANDROID_RELAY_PORT=$RELAY_PORT \
NMP_ANDROID_NIP46_SECRET=$nip46_pairing_secret \
    "$REPO_ROOT/target/release/android_controlled_relay" \
    > "$RELAY_LOG" 2>&1 &
relay_pid=$!

ready=0
for _ in $(seq 1 100); do
    if grep -q 'NMP_ANDROID_RELAY_READY' "$RELAY_LOG" 2>/dev/null; then
        ready=1
        break
    fi
    if ! kill -0 "$relay_pid" 2>/dev/null; then
        break
    fi
    sleep 0.1
done
if [[ "$ready" != 1 ]]; then
    echo "error: controlled relay did not become ready" >&2
    exit 1
fi
nip46_remote_pubkey=$(
    sed -n 's/.*remote_signer=\([^ ]*\).*/\1/p' "$RELAY_LOG" | head -n 1
)
if [[ ! "$nip46_remote_pubkey" =~ ^[0-9a-f]{64}$ ]]; then
    echo "error: controlled relay did not expose a valid NIP-46 remote pubkey" >&2
    exit 1
fi

# NIP-46's explicit bunker-session pool admits device loopback by design.
# Reverse that exact emulator port to the same host-owned relay rather than
# widening product admission to Android's emulator-only 10.0.2.2 gateway.
adb reverse "tcp:$RELAY_PORT" "tcp:$RELAY_PORT"
if ! adb reverse --list | grep -q "tcp:$RELAY_PORT tcp:$RELAY_PORT"; then
    echo "error: NIP-46 loopback reverse was not installed" >&2
    exit 1
fi

"$GRADLE" \
    --no-daemon \
    --console=plain \
    -p "$ANDROID_PROJECT" \
    publishReleasePublicationToQualificationRepository

echo "== positive: x86_64 AAR executes observation, cancellation, reopen, and close =="
"$GRADLE" \
    --no-daemon \
    --console=plain \
    -p "$CONSUMER" \
    -PnmpAndroidRepository="$QUALIFICATION_REPOSITORY" \
    -PnmpQualificationRelay="$RELAY_URL" \
    -PnmpNip46Relay="$NIP46_RELAY_URL" \
    -PnmpNip46RemotePubkey="$nip46_remote_pubkey" \
    :app:clean :app:connectedDebugAndroidTest \
    | tee "$ARTIFACTS/positive-instrumentation.txt"

if ! grep -q 'NMP_ANDROID_RELAY_REQ' "$RELAY_LOG"; then
    echo "error: Android run reported success without a controlled-relay REQ" >&2
    exit 1
fi

adb install -r "$CONSUMER/app/build/outputs/apk/debug/app-debug.apk"
adb install -r -t \
    "$CONSUMER/app/build/outputs/apk/androidTest/debug/app-debug-androidTest.apk"

run_process_phase() {
    local phase=$1
    local method=$2
    local output="$ARTIFACTS/process-$phase-instrumentation.txt"
    local phase_arguments=()
    if [[ "$phase" == seed ]]; then
        phase_arguments=(-e nmpNip46PairingSecret "$nip46_pairing_secret")
    fi
    adb shell am instrument \
        -w \
        -r \
        -e nmpProcessPhase "$phase" \
        "${phase_arguments[@]}" \
        -e class \
        "com.nmp.qualification.consumer.NMPAndroidProcessDeathQualificationTest#$method" \
        "com.nmp.qualification.consumer.test/androidx.test.runner.AndroidJUnitRunner" \
        | tee "$output"
    if grep -q 'FAILURES!!!' "$output" ||
        ! grep -q 'INSTRUMENTATION_CODE: -1' "$output"; then
        echo "error: Android process-death phase $phase failed" >&2
        exit 1
    fi
    adb shell am force-stop com.nmp.qualification.consumer
    if [[ -n "$(adb shell pidof com.nmp.qualification.consumer | tr -d '\r')" ]]; then
        echo "error: target process survived force-stop after phase $phase" >&2
        exit 1
    fi
}

echo "== process death: seed protected checkpoints and one durable receipt =="
run_process_phase seed seedProtectedCheckpointsAndDurableReceipt
echo "== process death: restore exact identity/session/receipt without publish =="
run_process_phase restore restoreIdentitySessionAndExactReceipt
echo "== process death: cleared credentials must not resurrect =="
run_process_phase verify-clear clearedCredentialsStayAbsentAfterAnotherProcessDeath

if grep -R -F -l -- "$nip46_pairing_secret" \
    "$ARTIFACTS" \
    "$CONSUMER/app/build/outputs" \
    "$ANDROID_PROJECT/build/outputs" >/dev/null; then
    echo "error: ephemeral NIP-46 pairing secret escaped into captured artifacts" >&2
    exit 1
fi

connect_count=$(grep -c 'NMP_ANDROID_NIP46_METHOD connect' "$RELAY_LOG" || true)
get_public_key_count=$(grep -c 'NMP_ANDROID_NIP46_METHOD get_public_key' "$RELAY_LOG" || true)
sign_event_count=$(grep -c 'NMP_ANDROID_NIP46_METHOD sign_event' "$RELAY_LOG" || true)
write_count=$(grep -c 'NMP_ANDROID_RELAY_WRITE' "$RELAY_LOG" || true)
if [[ "$connect_count" != 1 ||
    "$get_public_key_count" -lt 2 ||
    "$sign_event_count" != 1 ||
    "$write_count" != 1 ]]; then
    echo "error: process restore re-paired or re-published " \
        "(connect=$connect_count get_public_key=$get_public_key_count " \
        "sign=$sign_event_count write=$write_count)" >&2
    exit 1
fi

cp "$AAR" "$MISSING_ABI_AAR"
zip -q -d "$MISSING_ABI_AAR" 'jni/x86_64/libnmp_ffi.so'
unzip -l "$MISSING_ABI_AAR" > "$ARTIFACTS/missing-abi-aar-inventory.txt"

echo "== negative: AAR missing libnmp_ffi x86_64 must refuse NMPEngine =="
"$GRADLE" \
    --no-daemon \
    --console=plain \
    -p "$CONSUMER" \
    -PnmpAndroidRepository="$QUALIFICATION_REPOSITORY" \
    -PnmpQualificationRelay="$RELAY_URL" \
    -PnmpNip46Relay="$NIP46_RELAY_URL" \
    -PnmpNip46RemotePubkey="$nip46_remote_pubkey" \
    -PnmpMissingRuntimeAar="$MISSING_ABI_AAR" \
    :app:clean :app:connectedDebugAndroidTest \
    | tee "$ARTIFACTS/missing-abi-instrumentation.txt"

adb logcat -d -s NMPQualification:I '*:S' > "$ARTIFACTS/qualification-logcat.txt"
for marker in \
    NMP_ANDROID_ACTIVITY_LAUNCHED \
    NMP_ANDROID_OBSERVED \
    NMP_ANDROID_CANCELLED \
    NMP_ANDROID_UNAVAILABLE \
    NMP_ANDROID_CLOSED \
    NMP_ANDROID_REOPENED \
    NMP_ANDROID_LIFECYCLE_RECREATED \
    NMP_ANDROID_COLD_FLOW_HANDLES \
    NMP_ANDROID_LIFECYCLE_CLOSED \
    NMP_ANDROID_KEYSTORE_CIPHERTEXT \
    NMP_ANDROID_KEYSTORE_TAMPER \
    NMP_ANDROID_KEYSTORE_INVALIDATED \
    NMP_ANDROID_KEYSTORE_CONCURRENT \
    NMP_ANDROID_PROCESS_SEEDED \
    NMP_ANDROID_PROCESS_RESTORED \
    NMP_ANDROID_PROCESS_CLEARED \
    NMP_ANDROID_WRONG_ABI_REFUSED; do
    if ! grep -q "$marker" "$ARTIFACTS/qualification-logcat.txt"; then
        echo "error: missing runtime proof marker $marker" >&2
        exit 1
    fi
done

echo "test-android-emulator: supported facade runtime and negative controls passed"
