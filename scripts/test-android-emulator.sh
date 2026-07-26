#!/usr/bin/env bash
# Run #832's external AAR consumer on the already-booted governed emulator.
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
RELAY_LOG="$ARTIFACTS/controlled-relay.log"
GRADLE="$REPO_ROOT/Packages/NMPKotlin/gradlew"
AAR="$ANDROID_PROJECT/build/outputs/aar/NMPAndroid-release.aar"
MISSING_ABI_AAR="$ARTIFACTS/NMPAndroid-missing-x86_64.aar"

mkdir -p "$ARTIFACTS"

relay_pid=
capture_runtime_evidence() {
    adb logcat -d > "$ARTIFACTS/logcat.txt" 2>&1 || true
    adb shell getprop > "$ARTIFACTS/emulator-properties.txt" 2>&1 || true
    find "$CONSUMER/app/build/outputs" -type f -print \
        > "$ARTIFACTS/consumer-output-inventory.txt" 2>&1 || true
    while IFS= read -r apk; do
        unzip -l "$apk" >> "$ARTIFACTS/apk-inventory.txt" 2>&1 || true
    done < <(find "$CONSUMER/app/build/outputs" -type f -name '*.apk' -print 2>/dev/null)
    if [[ -n "$relay_pid" ]]; then
        kill "$relay_pid" 2>/dev/null || true
        wait "$relay_pid" 2>/dev/null || true
    fi
}
trap capture_runtime_evidence EXIT

{
    echo "java=$("$JAVA_HOME/bin/java" -version 2>&1 | head -n 1)"
    echo "adb=$(adb version | head -n 1)"
    echo "device=$(adb shell getprop ro.product.name | tr -d '\r')"
    echo "sdk=$(adb shell getprop ro.build.version.sdk | tr -d '\r')"
    echo "abi=$(adb shell getprop ro.product.cpu.abi | tr -d '\r')"
    echo "relay=$RELAY_URL"
} > "$ARTIFACTS/runtime-context.txt"
unzip -l "$AAR" > "$ARTIFACTS/aar-inventory.txt"

NMP_ANDROID_RELAY_PORT=$RELAY_PORT \
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
    :app:clean :app:connectedDebugAndroidTest \
    | tee "$ARTIFACTS/positive-instrumentation.txt"

if ! grep -q 'NMP_ANDROID_RELAY_REQ' "$RELAY_LOG"; then
    echo "error: Android run reported success without a controlled-relay REQ" >&2
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
    NMP_ANDROID_WRONG_ABI_REFUSED; do
    if ! grep -q "$marker" "$ARTIFACTS/qualification-logcat.txt"; then
        echo "error: missing runtime proof marker $marker" >&2
        exit 1
    fi
done

echo "test-android-emulator: supported facade runtime and negative controls passed"
