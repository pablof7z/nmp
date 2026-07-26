#!/usr/bin/env bash
# Inspect one NMP Android AAR and prove its declared ABI/facade/binding shape.

set -euo pipefail

if [[ $# -ne 2 ]]; then
    echo "usage: $0 <NMPAndroid-release.aar> <generated-nmp_ffi.kt>" >&2
    exit 2
fi

AAR=$1
BINDINGS=$2
[[ -f "$AAR" ]] || { echo "error: AAR not found: $AAR" >&2; exit 1; }
[[ -f "$BINDINGS" ]] || { echo "error: bindings not found: $BINDINGS" >&2; exit 1; }

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

unzip -q "$AAR" -d "$TMP/aar"
unzip -q "$TMP/aar/classes.jar" -d "$TMP/classes"

expected_abis=$(printf '%s\n' arm64-v8a x86_64)
actual_abis=$(
    find "$TMP/aar/jni" -mindepth 2 -maxdepth 2 -type f -name 'libnmp_ffi.so' \
        | sed 's#^.*/jni/\([^/]*\)/libnmp_ffi\.so$#\1#' \
        | LC_ALL=C sort
)
if [[ "$actual_abis" != "$expected_abis" ]]; then
    echo "error: AAR ABI set differs from the qualified matrix" >&2
    echo "expected:" >&2
    printf '%s\n' "$expected_abis" >&2
    echo "actual:" >&2
    printf '%s\n' "$actual_abis" >&2
    exit 1
fi

for class_file in \
    com/nmp/sdk/NMPEngine.class \
    com/nmp/sdk/NMPConfig.class \
    com/nmp/sdk/NMPAndroidCheckpointException.class \
    com/nmp/sdk/NMPAndroidKeyStoreAccountStore.class \
    com/nmp/sdk/NMPAndroidKeyStoreNip46SessionCheckpointStore.class \
    uniffi/nmp_ffi/NmpEngine.class; do
    if [[ ! -f "$TMP/classes/$class_file" ]]; then
        echo "error: AAR classes.jar is missing $class_file" >&2
        exit 1
    fi
done

for desktop_class in \
    com/nmp/sdk/NMPSecureKeyStoreAccountStore.class \
    com/nmp/sdk/NMPSecureKeyStoreNip46SessionCheckpointStore.class; do
    if [[ -e "$TMP/classes/$desktop_class" ]]; then
        echo "error: desktop-only JCEKS provider leaked into Android AAR: $desktop_class" >&2
        exit 1
    fi
done

if ! grep -q 'uniffiCheckApiChecksums' "$BINDINGS"; then
    echo "error: generated bindings do not contain UniFFI API checksum checks" >&2
    exit 1
fi

checksum_symbols=$(
    sed -n 's/^[[:space:]]*fun[[:space:]]\+\([A-Za-z0-9_]*checksum_[A-Za-z0-9_]*\).*/\1/p' "$BINDINGS" \
        | LC_ALL=C sort -u
)
if [[ -z "$checksum_symbols" ]]; then
    echo "error: no UniFFI checksum symbols found in generated bindings" >&2
    exit 1
fi

if [[ -n "${NMP_ANDROID_NM:-}" ]]; then
    LLVM_NM="$NMP_ANDROID_NM"
else
    LLVM_NM=$(
        find "${ANDROID_NDK_HOME:-/nonexistent}/toolchains/llvm/prebuilt" \
            -type f -path '*/bin/llvm-nm' -perm -111 2>/dev/null | head -n 1
    )
fi
if [[ -z "${LLVM_NM:-}" || ! -x "$LLVM_NM" ]]; then
    echo "error: set ANDROID_NDK_HOME or NMP_ANDROID_NM to an executable llvm-nm" >&2
    exit 1
fi

LLVM_READELF="${LLVM_NM%/llvm-nm}/llvm-readelf"
if [[ ! -x "$LLVM_READELF" ]]; then
    echo "error: llvm-readelf not found beside $LLVM_NM" >&2
    exit 1
fi

for abi in arm64-v8a x86_64; do
    library="$TMP/aar/jni/$abi/libnmp_ffi.so"
    symbols=$("$LLVM_NM" -D --defined-only "$library" | awk '{print $NF}')
    if ! grep -qx 'ffi_nmp_ffi_uniffi_contract_version' <<<"$symbols"; then
        echo "error: $abi native library lacks the UniFFI contract-version symbol" >&2
        exit 1
    fi
    while IFS= read -r checksum; do
        [[ -z "$checksum" ]] && continue
        if ! grep -qx "$checksum" <<<"$symbols"; then
            echo "error: $abi native library does not match generated binding symbol $checksum" >&2
            exit 1
        fi
    done <<<"$checksum_symbols"

    machine=$("$LLVM_READELF" --file-header "$library" | sed -n 's/^[[:space:]]*Machine:[[:space:]]*//p')
    case "$abi:$machine" in
        arm64-v8a:*AArch64*) ;;
        x86_64:*X86-64*|x86_64:*x86-64*) ;;
        *)
            echo "error: $abi contains the wrong ELF machine: ${machine:-unknown}" >&2
            exit 1
            ;;
    esac
done

echo "verify-android-aar: exact ABI matrix, supported facade, and UniFFI contract symbols verified"
