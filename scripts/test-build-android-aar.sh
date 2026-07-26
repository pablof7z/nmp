#!/usr/bin/env bash
# End-to-end source/AAR qualification for issue #831.

set -euo pipefail

REPO_ROOT=$(git rev-parse --show-toplevel)
cd "$REPO_ROOT"

AAR="$REPO_ROOT/Packages/NMPAndroid/build/outputs/aar/NMPAndroid-release.aar"
BINDINGS="$REPO_ROOT/Packages/NMPAndroid/src/main/kotlin/uniffi/nmp_ffi/nmp_ffi.kt"
ANDROID_PROJECT="$REPO_ROOT/Packages/NMPAndroid"
CONSUMER="$REPO_ROOT/fixtures/android-aar-consumer"

"$REPO_ROOT/scripts/build-android-aar.sh"
"$REPO_ROOT/scripts/verify-android-aar.sh" "$AAR" "$BINDINGS"

if grep -R -n --include='*.kt' 'uniffi\.nmp_ffi' "$CONSUMER/app/src"; then
    echo "error: qualification consumer bypasses com.nmp.sdk with a raw UniFFI import" >&2
    exit 1
fi

echo "== publish the AAR and dependency metadata to an isolated local repository =="
"$REPO_ROOT/Packages/NMPKotlin/gradlew" \
    --no-daemon \
    --console=plain \
    -p "$ANDROID_PROJECT" \
    publishReleasePublicationToQualificationRepository

QUALIFICATION_REPOSITORY="$ANDROID_PROJECT/build/qualification-repository"
echo "== compile a standalone Android consumer against the published AAR =="
"$REPO_ROOT/Packages/NMPKotlin/gradlew" \
    --no-daemon \
    --console=plain \
    -p "$CONSUMER" \
    -PnmpAndroidRepository="$QUALIFICATION_REPOSITORY" \
    :app:assembleDebug

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

echo "== falsifier: an AAR missing x86_64 must be rejected =="
mkdir -p "$TMP/incomplete"
unzip -q "$AAR" -d "$TMP/incomplete"
rm "$TMP/incomplete/jni/x86_64/libnmp_ffi.so"
(
    cd "$TMP/incomplete"
    zip -q -r "$TMP/incomplete.aar" .
)
if "$REPO_ROOT/scripts/verify-android-aar.sh" "$TMP/incomplete.aar" "$BINDINGS"; then
    echo "error: incomplete ABI control unexpectedly passed" >&2
    exit 1
fi

echo "== falsifier: generated bindings naming an absent native checksum must be rejected =="
cp "$BINDINGS" "$TMP/mismatched.kt"
checksum_candidates=$(
    sed -n 's/^[[:space:]]*fun[[:space:]]\+\([A-Za-z0-9_]*checksum_[A-Za-z0-9_]*\).*/\1/p' "$BINDINGS"
)
first_checksum=${checksum_candidates%%$'\n'*}
if [[ -z "$first_checksum" ]]; then
    echo "error: could not select a checksum for the mismatch control" >&2
    exit 1
fi
sed "s/$first_checksum/${first_checksum}_mismatched/" "$BINDINGS" > "$TMP/mismatched.kt"
if "$REPO_ROOT/scripts/verify-android-aar.sh" "$AAR" "$TMP/mismatched.kt"; then
    echo "error: binding/native mismatch control unexpectedly passed" >&2
    exit 1
fi

echo "test-build-android-aar: clean external consumer and both negative controls passed"
