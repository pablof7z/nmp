#!/usr/bin/env bash
# Build the source-reproducible NMP Android AAR for issue #831.
#
# Required, pinned tools:
#   JDK 17
#   Android SDK platform 35
#   Android NDK 27.2.12479018
#   cargo-ndk 4.1.2
#   Rust Android targets aarch64-linux-android and x86_64-linux-android
#
# CARGO_TARGET_DIR is honored when supplied by the caller.

set -euo pipefail

REPO_ROOT=$(git rev-parse --show-toplevel)
cd "$REPO_ROOT"

CRATE=nmp-ffi
ANDROID_PROJECT="$REPO_ROOT/Packages/NMPAndroid"
ANDROID_BINDINGS="$ANDROID_PROJECT/src/main/kotlin/uniffi/nmp_ffi"
ANDROID_JNI="$ANDROID_PROJECT/src/main/jniLibs"
GEN_DIR="$REPO_ROOT/gen-android-kotlin"
NDK_VERSION=27.2.12479018
CARGO_NDK_VERSION=4.1.2
ANDROID_API=26

actual_cargo_ndk=$(cargo ndk --version | awk '{print $2}')
if [[ "$actual_cargo_ndk" != "$CARGO_NDK_VERSION" ]]; then
    echo "error: cargo-ndk $CARGO_NDK_VERSION is required; found ${actual_cargo_ndk:-unknown}" >&2
    exit 1
fi

if [[ -z "${JAVA_HOME:-}" || ! -x "$JAVA_HOME/bin/java" ]]; then
    echo "error: JAVA_HOME must name a JDK 17 installation" >&2
    exit 1
fi
java_major=$("$JAVA_HOME/bin/java" -version 2>&1 | sed -n '1s/.*version "\([0-9]*\).*/\1/p')
if [[ "$java_major" != "17" ]]; then
    echo "error: JDK 17 is required; JAVA_HOME reports major ${java_major:-unknown}" >&2
    exit 1
fi

if [[ -z "${ANDROID_HOME:-}" || ! -d "$ANDROID_HOME/platforms/android-35" ]]; then
    echo "error: ANDROID_HOME must contain platforms/android-35" >&2
    exit 1
fi

ANDROID_NDK_HOME=${NMP_ANDROID_NDK_HOME:-"$ANDROID_HOME/ndk/$NDK_VERSION"}
if [[ ! -f "$ANDROID_NDK_HOME/source.properties" ]]; then
    echo "error: Android NDK $NDK_VERSION not found at $ANDROID_NDK_HOME" >&2
    exit 1
fi
actual_ndk=$(sed -n 's/^Pkg.Revision[[:space:]]*=[[:space:]]*//p' "$ANDROID_NDK_HOME/source.properties")
if [[ "$actual_ndk" != "$NDK_VERSION" ]]; then
    echo "error: Android NDK $NDK_VERSION is required; found ${actual_ndk:-unknown}" >&2
    exit 1
fi
export ANDROID_NDK_HOME

TARGET_DIR_VALUE=${CARGO_TARGET_DIR:-target}
if [[ "$TARGET_DIR_VALUE" == /* ]]; then
    TARGET_DIR="$TARGET_DIR_VALUE"
else
    TARGET_DIR="$REPO_ROOT/$TARGET_DIR_VALUE"
fi

case "$(uname -s)" in
    Darwin) HOST_LIB_NAME=libnmp_ffi.dylib ;;
    Linux) HOST_LIB_NAME=libnmp_ffi.so ;;
    *)
        echo "error: binding generation supports macOS and Linux hosts only" >&2
        exit 1
        ;;
esac

echo "== 1. build host nmp-ffi and UniFFI generator =="
cargo build -p "$CRATE" --release

HOST_LIB="$TARGET_DIR/release/$HOST_LIB_NAME"
BINDGEN="$TARGET_DIR/release/uniffi-bindgen"
if [[ ! -f "$HOST_LIB" || ! -x "$BINDGEN" ]]; then
    echo "error: expected $HOST_LIB and $BINDGEN after the host build" >&2
    exit 1
fi

echo "== 2. generate Android Kotlin bindings from that exact host library =="
rm -rf "$GEN_DIR" "$ANDROID_BINDINGS"
mkdir -p "$GEN_DIR" "$ANDROID_BINDINGS"
"$BINDGEN" generate \
    --library "$HOST_LIB" \
    --language kotlin \
    --config "$ANDROID_PROJECT/uniffi.toml" \
    --out-dir "$GEN_DIR"
cp "$GEN_DIR/uniffi/nmp_ffi/nmp_ffi.kt" "$ANDROID_BINDINGS/"

echo "== 3. cross-compile the explicit Android ABI matrix at API $ANDROID_API =="
rm -rf "$ANDROID_JNI"
mkdir -p "$ANDROID_JNI"
cargo ndk \
    --target arm64-v8a \
    --target x86_64 \
    --platform "$ANDROID_API" \
    --output-dir "$ANDROID_JNI" \
    build -p "$CRATE" --lib --release

echo "== 4. assemble the release AAR =="
"$REPO_ROOT/Packages/NMPKotlin/gradlew" \
    --no-daemon \
    --console=plain \
    -p "$ANDROID_PROJECT" \
    clean assembleRelease

AAR="$ANDROID_PROJECT/build/outputs/aar/NMPAndroid-release.aar"
if [[ ! -f "$AAR" ]]; then
    echo "error: expected Android AAR at $AAR" >&2
    exit 1
fi

echo "== done =="
echo "Android AAR: $AAR"
echo "Bindings:   $ANDROID_BINDINGS/nmp_ffi.kt"
echo "ABIs:       arm64-v8a, x86_64"
