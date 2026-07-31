#!/usr/bin/env bash
set -euo pipefail

SCRIPT=$(cd "$(dirname "$0")" && pwd)/build-swift-xcframework.sh
CHECKER=$(cd "$(dirname "$0")" && pwd)/check-macos-deployment-target.sh
TOOL_HELPER=$(cd "$(dirname "$0")" && pwd)/lib/require-commands.sh
COMPONENT_BUILDER=$(cd "$(dirname "$0")" && pwd)/build-component-release.sh
PAIR_BUILDER=$(cd "$(dirname "$0")" && pwd)/build-swift-nip46-xcframework.sh
MANIFEST_VERIFIER=$(cd "$(dirname "$0")" && pwd)/verify-component-manifests.py
TMP=$(mktemp -d)
cleanup() {
  chmod -R u+w "$TMP" 2>/dev/null || true
  rm -r "$TMP"
}
trap cleanup EXIT

REPO="$TMP/repo"
BIN="$TMP/bin"
FIXTURE_SYSROOT="$TMP/rust-sysroot"
mkdir -p \
  "$REPO/scripts/lib" \
  "$REPO/Packages/NMP" \
  "$REPO/Packages/NMPNip46" \
  "$REPO/tools/component-artifact-witness" \
  "$FIXTURE_SYSROOT/lib/rustlib/x86_64-unknown-linux-gnu/bin" \
  "$BIN"
cp "$SCRIPT" "$REPO/scripts/"
cp "$PAIR_BUILDER" "$REPO/scripts/"
cp "$CHECKER" "$REPO/scripts/"
cp "$TOOL_HELPER" "$REPO/scripts/lib/"
cp "$COMPONENT_BUILDER" "$REPO/scripts/"
cp "$MANIFEST_VERIFIER" "$REPO/scripts/"
touch "$REPO/tools/component-artifact-witness/Cargo.toml"
git -C "$REPO" init -q

cat > "$REPO/Packages/NMP/Package.swift" <<'SWIFT'
// swift-tools-version: 5.9
import PackageDescription
let package = Package(
    name: "Fixture",
    platforms: [
        .macOS(.v13),
    ]
)
SWIFT

cat > "$BIN/component-artifact-witness-fixture" <<'PY'
#!/usr/bin/env python3
import hashlib
import json
import pathlib
import sys

def canonical(value):
    return json.dumps(value, separators=(",", ":"), sort_keys=True) + "\n"

args = sys.argv[2:]
options = {}
while args:
    name = args.pop(0)
    options[name] = args.pop(0)

command = sys.argv[1]
if command == "digest":
    print(hashlib.sha256(pathlib.Path(options["--file"]).read_bytes()).hexdigest())
elif command == "plan-localization":
    artifact = pathlib.Path(options["--artifact"])
    symbol = b"nmp_component_interface_fixture\0"
    pathlib.Path(options["--out"]).write_bytes(symbol)
    print(canonical({
        "artifact_blake3": hashlib.sha256(artifact.read_bytes()).hexdigest(),
        "interface_namespace": options["--interface-namespace"],
        "schema": 1,
        "symbols": ["nmp_component_interface_fixture"],
    }), end="")
elif command == "plan-authoritative-callables":
    artifact = pathlib.Path(options["--artifact"])
    namespace = (
        "nmp_ffi"
        if options["--component-key"] == "nmp-core"
        else "nmp_nip46_ffi"
    )
    callable_name = namespace + "_fixture_call"
    pathlib.Path(options["--out"]).write_bytes(callable_name.encode() + b"\0")
    print(canonical({
        "artifact_blake3": hashlib.sha256(artifact.read_bytes()).hexdigest(),
        "component_key": options["--component-key"],
        "schema": 1,
        "symbols": [callable_name],
        "uniffi_namespace": namespace,
    }), end="")
elif command == "witness":
    artifact = pathlib.Path(options["--artifact"])
    manifest_path = artifact.parent / "component-manifest.json"
    if manifest_path.is_file():
        manifest = json.loads(manifest_path.read_text())
    else:
        target = options["--target"]
        key = options["--component-key"]
        digest = lambda value: hashlib.sha256(value.encode()).hexdigest()
        if key == "nmp-core":
            package = "nmp-ffi"
            kind = "core"
            stem = namespace = "nmp_ffi"
            prefix = "nmp-core-component-v2"
            symbol = "NMP_CORE_COMPONENT_ATTESTATION_V2"
        else:
            package = "nmp-nip46-ffi"
            kind = "optional"
            stem = namespace = "nmp_nip46_ffi"
            prefix = "nmp-nip46-component-v2"
            symbol = "NMP_NIP46_COMPONENT_ATTESTATION_V2"
        identity = prefix + "-" + digest(prefix + "\0" + target)
        manifest = {
            "attestation_symbol": symbol,
            "binding_identity": identity,
            "build_flags_digest": digest("fixture-flags"),
            "cargo_package": package,
            "component_key": key,
            "graph_digest": digest("fixture-graph-" + key),
            "identity": identity,
            "interface_identity": (
                "nmp-component-interface-v2-" + digest("fixture-interface")
            ),
            "kind": kind,
            "library_stem": stem,
            "native_identity": identity,
            "profile": "release",
            "rustc_digest": digest("fixture-rustc"),
            "schema": 2,
            "target": target,
            "uniffi_namespace": namespace,
        }
        if kind == "optional":
            core_identity = (
                "nmp-core-component-v2-"
                + digest("nmp-core-component-v2\0" + target)
            )
            core_manifest = {
                "attestation_symbol": "NMP_CORE_COMPONENT_ATTESTATION_V2",
                "binding_identity": core_identity,
                "build_flags_digest": digest("fixture-flags"),
                "cargo_package": "nmp-ffi",
                "component_key": "nmp-core",
                "graph_digest": digest("fixture-graph-nmp-core"),
                "identity": core_identity,
                "interface_identity": manifest["interface_identity"],
                "kind": "core",
                "library_stem": "nmp_ffi",
                "native_identity": core_identity,
                "profile": "release",
                "rustc_digest": digest("fixture-rustc"),
                "schema": 2,
                "target": target,
                "uniffi_namespace": "nmp_ffi",
            }
            core_manifest_bytes = canonical(core_manifest).encode()
            core_symbol = b"nmp_component_interface_fixture"
            core_strings = b"\0" + core_symbol + b"\0"
            core_header = __import__("struct").pack(
                "<8I", 0xFEEDFACF, 0x0100000C, 0, 1, 1, 24, 0, 0
            )
            core_symtab = __import__("struct").pack(
                "<6I", 2, 24, 56, 1, 72, len(core_strings)
            )
            core_nlist = __import__("struct").pack("<IBBHQ", 1, 0x0F, 1, 0, 0)
            core_payload = core_header + core_symtab + core_nlist + core_strings
            core_member = (
                b"fixture.o/      "
                + b"0           "
                + b"0     "
                + b"0     "
                + b"100644  "
                + str(len(core_payload)).encode().ljust(10)
                + b"`\n"
            )
            core_archive = b"!<arch>\n" + core_member + core_payload
            if len(core_payload) % 2:
                core_archive += b"\n"
            manifest.update({
                "required_core_artifact_blake3": hashlib.sha256(core_archive).hexdigest(),
                "required_core_identity": core_identity,
                "required_core_manifest_blake3": hashlib.sha256(core_manifest_bytes).hexdigest(),
            })
    attestation_fields = {
        "build_flags_digest",
        "cargo_package",
        "component_key",
        "graph_digest",
        "identity",
        "interface_identity",
        "kind",
        "library_stem",
        "profile",
        "rustc_digest",
        "target",
        "uniffi_namespace",
    }
    if manifest["kind"] == "optional":
        attestation_fields |= {
            "required_core_artifact_blake3",
            "required_core_identity",
            "required_core_manifest_blake3",
        }
    attestation = {field: manifest[field] for field in attestation_fields}
    attestation["schema"] = 1
    callable_name = manifest["uniffi_namespace"] + "_fixture_call"
    suffix = artifact.suffix
    if "-apple-" in manifest["target"]:
        artifact_format = (
            "macho-dylib" if suffix == ".dylib" else "archive-macho"
        )
    elif suffix == ".a":
        artifact_format = "archive-elf"
    elif suffix == ".dylib":
        artifact_format = "macho-dylib"
    else:
        artifact_format = "elf-shared-object"
    public_callable = (
        "_" + callable_name if "macho" in artifact_format else callable_name
    )
    print(canonical({
        "architecture": "aarch64",
        "artifact_blake3": hashlib.sha256(artifact.read_bytes()).hexdigest(),
        "artifact_size": artifact.stat().st_size,
        "attestation": attestation,
        "component_key": manifest["component_key"],
        "format": artifact_format,
        "public_symbols": [public_callable],
        "schema": 1,
        "target": manifest["target"],
        "uniffi_components": [{
            "callables": [callable_name],
            "namespace": manifest["uniffi_namespace"],
        }],
    }), end="")
else:
    raise SystemExit(64)
PY

cat > "$BIN/rustc" <<'SHIM'
#!/usr/bin/env bash
set -euo pipefail
case "${1:-}" in
  --print)
    [[ ${2:-} == sysroot ]]
    printf '%s\n' "$FIXTURE_SYSROOT"
    ;;
  -vV)
    printf '%s\n' 'rustc 1.0.0 (fixture)' 'host: x86_64-unknown-linux-gnu'
    ;;
  *) exit 64 ;;
esac
SHIM

cat > "$FIXTURE_SYSROOT/lib/rustlib/x86_64-unknown-linux-gnu/bin/rust-objcopy" <<'SHIM'
#!/usr/bin/env bash
set -euo pipefail
artifact=
for argument in "$@"; do artifact=$argument; done
[[ -f "$artifact" ]]
SHIM

cat > "$BIN/ranlib" <<'SHIM'
#!/usr/bin/env bash
set -euo pipefail
[[ -f ${1:-} ]]
SHIM

cat > "$BIN/cargo" <<'SHIM'
#!/usr/bin/env bash
set -euo pipefail
printf 'cargo' >> "$CALL_LOG"
printf ' %q' "$@" >> "$CALL_LOG"
printf ' deployment=%s' "${MACOSX_DEPLOYMENT_TARGET-unset}" >> "$CALL_LOG"
printf ' cflags=%q' "${CFLAGS-unset}" >> "$CALL_LOG"
printf ' cxxflags=%q' "${CXXFLAGS-unset}" >> "$CALL_LOG"
printf '\n' >> "$CALL_LOG"

case "${1:-}" in
  fetch)
    ;;
  metadata)
    cat <<'JSON'
{"packages":[{"name":"nmp-ffi","metadata":{"nmp-component":{"bindgen-bin":"uniffi-bindgen","key":"nmp-core","kind":"core","library-stem":"nmp_ffi","schema":1,"uniffi-namespace":"nmp_ffi"}}},{"name":"nmp-nip46-ffi","metadata":{"nmp-component":{"bindgen-bin":"nmp-nip46-uniffi-bindgen","key":"nmp-nip46","kind":"optional","library-stem":"nmp_nip46_ffi","metadata-audit-bin":"nmp-nip46-metadata-audit","schema":1,"uniffi-namespace":"nmp_nip46_ffi"}}}]}
JSON
    ;;
  build)
    if [[ " $* " == *" --manifest-path "*"component-artifact-witness/Cargo.toml"* ]]; then
      target_dir=
      while [[ $# -gt 0 ]]; do
        case "$1" in
          --target-dir) target_dir=$2; shift 2 ;;
          *) shift ;;
        esac
      done
      [[ -n $target_dir ]]
      mkdir -p "$target_dir/release"
      cp "$FIXTURE_WITNESS_TOOL" \
        "$target_dir/release/nmp-component-artifact-witness"
      chmod +x "$target_dir/release/nmp-component-artifact-witness"
      exit 0
    fi
    if [[ " $* " == *" --bin nmp-nip46-metadata-audit "* ]]; then
      audit_target_dir=
      while [[ $# -gt 0 ]]; do
        case "$1" in
          --target-dir) audit_target_dir=$2; shift 2 ;;
          *) shift ;;
        esac
      done
      [[ -n $audit_target_dir ]]
      mkdir -p "$audit_target_dir/debug"
      cat >"$audit_target_dir/debug/nmp-nip46-metadata-audit" <<'AUDIT'
#!/usr/bin/env bash
set -euo pipefail
[[ -r ${1:-} ]]
AUDIT
      chmod +x "$audit_target_dir/debug/nmp-nip46-metadata-audit"
      exit 0
    fi
    package=
    target=
    while [[ $# -gt 0 ]]; do
      case "$1" in
        -p) package=$2; shift 2 ;;
        --target) target=$2; shift 2 ;;
        *) shift ;;
      esac
    done
    [[ -n $package && -n $target ]]
    mkdir -p "$CARGO_TARGET_DIR/$target/release"
    if [[ $package == nmp-ffi ]]; then
      component_key=nmp-core
      kind=core
      library_stem=nmp_ffi
      namespace=nmp_ffi
      identity_prefix=nmp-core-component-v2
      attestation_symbol=NMP_CORE_COMPONENT_ATTESTATION_V2
    else
      component_key=nmp-nip46
      kind=optional
      library_stem=nmp_nip46_ffi
      namespace=nmp_nip46_ffi
      identity_prefix=nmp-nip46-component-v2
      attestation_symbol=NMP_NIP46_COMPONENT_ATTESTATION_V2
    fi
    library="$CARGO_TARGET_DIR/$target/release/lib$library_stem.a"
    python3 - "$library" <<'PY'
import pathlib
import struct
import sys

symbol = b"nmp_component_interface_fixture"
strings = b"\0" + symbol + b"\0"
header = struct.pack("<8I", 0xFEEDFACF, 0x0100000C, 0, 1, 1, 24, 0, 0)
symtab = struct.pack("<6I", 2, 24, 56, 1, 72, len(strings))
nlist = struct.pack("<IBBHQ", 1, 0x0F, 1, 0, 0)
payload = header + symtab + nlist + strings
member_header = (
    b"fixture.o/      "
    + b"0           "
    + b"0     "
    + b"0     "
    + b"100644  "
    + str(len(payload)).encode().ljust(10)
    + b"`\n"
)
archive = b"!<arch>\n" + member_header + payload
if len(payload) % 2:
    archive += b"\n"
pathlib.Path(sys.argv[1]).write_bytes(archive)
PY
    python3 - \
      "$NMP_COMPONENT_MANIFEST_OUTPUT" "$package" "$component_key" "$kind" \
      "$library_stem" "$namespace" "$identity_prefix" "$attestation_symbol" \
      "$target" "${NMP_COMPONENT_CORE_ARTIFACT:-}" <<'PY'
import hashlib
import json
import pathlib
import sys

(output, package, key, kind, stem, namespace, prefix, symbol, target, core) = sys.argv[1:]
digest = lambda value: hashlib.sha256(value.encode()).hexdigest()
identity = prefix + "-" + digest(prefix + "\0" + target)
value = {
    "attestation_symbol": symbol,
    "binding_identity": identity,
    "build_flags_digest": digest("fixture-flags"),
    "cargo_package": package,
    "component_key": key,
    "graph_digest": digest("fixture-graph-" + key),
    "identity": identity,
    "interface_identity": "nmp-component-interface-v2-" + digest("fixture-interface"),
    "kind": kind,
    "library_stem": stem,
    "native_identity": identity,
    "profile": "release",
    "rustc_digest": digest("fixture-rustc"),
    "schema": 2,
    "target": target,
    "uniffi_namespace": namespace,
}
if kind == "optional":
    core_path = pathlib.Path(core)
    core_manifest_path = core_path.parent / "component-manifest.json"
    core_manifest = json.loads(core_manifest_path.read_text())
    value.update({
        "required_core_artifact_blake3": hashlib.sha256(core_path.read_bytes()).hexdigest(),
        "required_core_identity": core_manifest["identity"],
        "required_core_manifest_blake3": hashlib.sha256(core_manifest_path.read_bytes()).hexdigest(),
    })
pathlib.Path(output).write_text(
    json.dumps(value, separators=(",", ":"), sort_keys=True) + "\n"
)
PY
    ;;
  run)
    if [[ " $* " == *" nmp-nip46-metadata-audit "* ]]; then
      exit 0
    fi
    out_dir=
    library=
    while [[ $# -gt 0 ]]; do
      case "$1" in
        --out-dir) out_dir=$2; shift 2 ;;
        --library) library=$2; shift 2 ;;
        *) shift ;;
      esac
    done
    [[ -f $library ]]
    stem=$(basename "$library")
    stem=${stem#lib}
    stem=${stem%.*}
    mkdir -p "$out_dir"
    printf '%s\n' 'import Foundation' > "$out_dir/$stem.swift"
    : > "$out_dir/${stem}FFI.h"
    : > "$out_dir/${stem}FFI.modulemap"
    if [[ $stem == nmp_ffi ]]; then
      printf '%s\n' 'import Foundation' \
        > "$out_dir/nmp_component_interface.swift"
      : > "$out_dir/nmp_component_interfaceFFI.h"
      : > "$out_dir/nmp_component_interfaceFFI.modulemap"
    fi
    ;;
  *) exit 64 ;;
esac
SHIM

cat > "$BIN/otool" <<'SHIM'
#!/usr/bin/env bash
set -euo pipefail
printf 'otool' >> "$CALL_LOG"
printf ' %q' "$@" >> "$CALL_LOG"
printf '\n' >> "$CALL_LOG"
artifact=
for arg in "$@"; do artifact=$arg; done
case "${OTOOL_FIXTURE:-valid}" in
  valid)
    cat <<EOF
Archive : $artifact
$artifact(first.o):
Load command 0
      cmd LC_BUILD_VERSION
 platform 1
    minos 13.0
$artifact(second.o):
Load command 0
      cmd LC_BUILD_VERSION
 platform 1
    minos 11.0
EOF
    ;;
  newer)
    cat <<EOF
Archive : $artifact
$artifact(newer.o):
Load command 0
      cmd LC_BUILD_VERSION
 platform 1
    minos 14.0
EOF
    ;;
  exact-old)
    cat <<EOF
$artifact:
Load command 0
      cmd LC_BUILD_VERSION
 platform 1
    minos 12.0
EOF
    ;;
  exact)
    cat <<EOF
$artifact:
Load command 0
      cmd LC_BUILD_VERSION
 platform 1
    minos 13.0
EOF
    ;;
  missing)
    cat <<EOF
Archive : $artifact
$artifact(no-target.o):
Load command 0
      cmd LC_SEGMENT_64
EOF
    ;;
  *) exit 64 ;;
esac
if [[ ${OTOOL_INODE_SWAP:-0} == 1 ]]; then
  artifact_dir=$(dirname "$artifact")
  chmod u+w "$artifact_dir"
  cp "$artifact" "$artifact.swap"
  chmod u+w "$artifact.swap"
  python3 - "$artifact.swap" <<'PY'
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
data = bytearray(path.read_bytes())
data[-1] = ord("X") if data[-1] != ord("X") else ord("Y")
path.write_bytes(data)
PY
  mv -f "$artifact.swap" "$artifact"
  chmod a-w "$artifact" "$artifact_dir"
fi
SHIM

cat > "$BIN/lipo" <<'SHIM'
#!/usr/bin/env bash
set -euo pipefail
printf 'lipo' >> "$CALL_LOG"
printf ' %q' "$@" >> "$CALL_LOG"
printf '\n' >> "$CALL_LOG"
if [[ ${1:-} == -create ]]; then
  while [[ $# -gt 0 ]]; do
    if [[ $1 == -output ]]; then
      mkdir -p "$(dirname "$2")"
      : > "$2"
      break
    fi
    shift
  done
fi
SHIM

cat > "$BIN/xcodebuild" <<'SHIM'
#!/usr/bin/env bash
set -euo pipefail
printf 'xcodebuild' >> "$CALL_LOG"
printf ' %q' "$@" >> "$CALL_LOG"
printf '\n' >> "$CALL_LOG"
library=
headers=
output=
while [[ $# -gt 0 ]]; do
  case "$1" in
    -library) library=$2; shift 2 ;;
    -headers) headers=$2; shift 2 ;;
    -output) output=$2; shift 2 ;;
    *) shift ;;
  esac
done
[[ -f $library && -d $headers && -n $output ]]
mkdir -p "$output/macos-arm64/Headers"
cp "$library" "$output/macos-arm64/"
cp "$headers/"* "$output/macos-arm64/Headers/"
printf '%s\n' 'fixture xcframework' >"$output/Info.plist"
SHIM
chmod +x "$BIN/"*
chmod +x \
  "$FIXTURE_SYSROOT/lib/rustlib/x86_64-unknown-linux-gnu/bin/rust-objcopy"
chmod +x "$REPO/scripts/"*.sh

run_script() {
  local log=$1 target_dir=$2
  shift 2
  : > "$log"
  (
    cd "$REPO"
    if [[ -n ${NMP_COMPONENT_VERIFIER_HOOK_DIR:-} ]]; then
      export NMP_COMPONENT_VERIFIER_HOOK_DIR
    fi
    if [[ -n ${OTOOL_INODE_SWAP:-} ]]; then
      export OTOOL_INODE_SWAP
    fi
    PATH="$BIN:$PATH" \
      CALL_LOG="$log" \
      CARGO_TARGET_DIR="$target_dir" \
      FIXTURE_SYSROOT="$FIXTURE_SYSROOT" \
      FIXTURE_WITNESS_TOOL="$BIN/component-artifact-witness-fixture" \
      MACOSX_DEPLOYMENT_TARGET=99.0 \
      CFLAGS=-mmacosx-version-min=99.0 \
      CXXFLAGS=-mmacosx-version-min=99.0 \
      scripts/build-swift-xcframework.sh "$@"
  )
}

wait_for_hook() {
  local pid=$1 ready=$2 output=$3
  local attempt
  for attempt in {1..1000}; do
    [[ -e $ready ]] && return
    if ! kill -0 "$pid" 2>/dev/null; then
      wait "$pid" || true
      echo "fixture exited before hook: $ready" >&2
      cat "$output" >&2
      exit 1
    fi
    sleep 0.01
  done
  kill "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
  echo "timed out waiting for hook: $ready" >&2
  exit 1
}

assert_no_calls() {
  [[ ! -s $1 ]] || {
    echo "unexpected tool call:" >&2
    cat "$1" >&2
    exit 1
  }
}

# Help and invalid input must finish before repository discovery or any tool.
help_log="$TMP/help.log"
: > "$help_log"
help_output=$(cd / && PATH="$BIN:$PATH" CALL_LOG="$help_log" \
  "$REPO/scripts/build-swift-xcframework.sh" --help)
grep -Fq -- '--macos-only' <<< "$help_output"
assert_no_calls "$help_log"

for args in '--unknown' '--sim-only --macos-only' 'positional'; do
  invalid_log="$TMP/invalid.log"
  : > "$invalid_log"
  if (cd / && PATH="$BIN:$PATH" CALL_LOG="$invalid_log" \
    "$REPO/scripts/build-swift-xcframework.sh" $args >/dev/null 2>&1); then
    echo "invalid invocation unexpectedly passed: $args" >&2
    exit 1
  fi
  assert_no_calls "$invalid_log"
done
echo 'ok - help and argument rejection are side-effect-free'

# macOS-only uses the shared target directory for build, bindgen input,
# headers, and the one xcframework library. It performs no simulator work.
mac_log="$TMP/macos.log"
shared_target="$TMP/shared-cache"
run_script "$mac_log" "$shared_target" --macos-only >/dev/null
grep -Fq 'cargo build --frozen -p nmp-ffi --release --target aarch64-apple-darwin' "$mac_log"
grep -Fq -- '--target aarch64-apple-darwin deployment=13.0' "$mac_log"
grep -Fq 'cflags=-mmacosx-version-min=99.0\ -mmacosx-version-min=13.0' "$mac_log"
grep -Fq 'cxxflags=-mmacosx-version-min=99.0\ -mmacosx-version-min=13.0' "$mac_log"
grep -Fq "$shared_target/nmp-component-artifacts-v2/nmp-core/aarch64-apple-darwin." "$mac_log"
grep -Fq "$shared_target/ios-ffi-headers" "$mac_log"
! grep -Fq 'apple-ios' "$mac_log"
! grep -Fq 'lipo' "$mac_log"
[[ $(grep -c '^xcodebuild ' "$mac_log") -eq 1 ]]
echo 'ok - macOS-only plan uses the caller target directory and no simulator'

# Relative CARGO_TARGET_DIR resolves from the repository root for both Cargo
# and packaging lookups.
relative_log="$TMP/relative.log"
run_script "$relative_log" relative-target --macos-only >/dev/null
grep -Fq "$REPO/relative-target/nmp-component-artifacts-v2/nmp-core/aarch64-apple-darwin." "$relative_log"
echo 'ok - relative CARGO_TARGET_DIR artifact lookup matches Cargo'

# The paired builder gets both matched libraries from one managed build for
# the selected target, then packages two XCFrameworks from those same sealed
# snapshots. A second core Cargo build or simulator target would defeat the
# per-PR design this path exists to support.
pair_log="$TMP/pair.log"
pair_target="$TMP/pair-target"
: > "$pair_log"
(
  cd "$REPO"
  PATH="$BIN:$PATH" \
    CALL_LOG="$pair_log" \
    CARGO_TARGET_DIR="$pair_target" \
    FIXTURE_SYSROOT="$FIXTURE_SYSROOT" \
    FIXTURE_WITNESS_TOOL="$BIN/component-artifact-witness-fixture" \
    scripts/build-swift-nip46-xcframework.sh --macos-only
) >/dev/null
[[ $(grep -c 'cargo build --frozen -p nmp-ffi --release --target aarch64-apple-darwin' "$pair_log") -eq 1 ]]
[[ $(grep -c 'cargo build --frozen -p nmp-nip46-ffi --release --target aarch64-apple-darwin' "$pair_log") -eq 1 ]]
grep -Fq "$pair_target/nmp-component-artifacts-v2/nmp-nip46/aarch64-apple-darwin." \
  "$pair_log"
grep -Fq 'libnmp_ffi.a' "$pair_log"
grep -Fq 'libnmp_nip46_ffi.a' "$pair_log"
[[ $(grep -c '^xcodebuild ' "$pair_log") -eq 2 ]]
! grep -Fq 'apple-ios' "$pair_log"
grep -Fq 'import NMPFFI' \
  "$REPO/Packages/NMPNip46/Sources/NMPNip46FFI/nmp_nip46_ffi.swift"
grep -Eq \
  'nmpNip46PackagedComponentIdentity = "nmp-nip46-component-v2-[0-9a-f]{64}"' \
  "$REPO/Packages/NMPNip46/Sources/NMPNip46FFI/nmp_nip46_ffi.swift"
core_xcframework="$REPO/Packages/NMP/NMP.xcframework"
provider_xcframework="$REPO/Packages/NMPNip46/NMPNip46.xcframework"
[[ -f $core_xcframework/macos-arm64/libnmp_ffi.a ]]
[[ -f $provider_xcframework/macos-arm64/libnmp_nip46_ffi.a ]]
[[ -f $core_xcframework/Info.plist ]]
[[ -f $provider_xcframework/Info.plist ]]
[[ ! -w $core_xcframework/macos-arm64/libnmp_ffi.a ]]
[[ ! -w $provider_xcframework/macos-arm64/libnmp_nip46_ffi.a ]]
! find "$core_xcframework" "$provider_xcframework" -type l -print -quit |
  grep -q .
echo 'ok - paired macOS-only build compiles once and packages both components'

# Preserve the historical sim-only and default target sets.
sim_log="$TMP/sim.log"
run_script "$sim_log" "$TMP/sim-target" --sim-only >/dev/null
grep -Fq -- '--target aarch64-apple-ios-sim' "$sim_log"
grep -Fq -- '--target x86_64-apple-ios' "$sim_log"
grep -Fq -- '--target aarch64-apple-darwin' "$sim_log"
grep -Fq -- '--target aarch64-apple-ios-sim deployment=unset' "$sim_log"
grep -Fq -- '--target x86_64-apple-ios deployment=unset' "$sim_log"
grep -Fq -- '--target aarch64-apple-darwin deployment=13.0' "$sim_log"
! grep -Fq -- '--target aarch64-apple-ios deployment=' "$sim_log"
grep -Fq 'lipo -create' "$sim_log"

default_log="$TMP/default.log"
run_script "$default_log" "$TMP/default-target" >/dev/null
grep -Fq -- '--target aarch64-apple-ios-sim' "$default_log"
grep -Fq -- '--target x86_64-apple-ios' "$default_log"
grep -Fq -- '--target aarch64-apple-darwin' "$default_log"
grep -Fq -- '--target aarch64-apple-ios deployment=unset' "$default_log"
grep -Fq 'lipo -create' "$default_log"
echo 'ok - sim-only and default target sets remain compatible'

# The standalone checker derives the package minimum, accepts older members,
# rejects one newer member or one missing load command, and can require the
# final linked image to encode the exact declared minimum.
checker_log="$TMP/checker.log"
: > "$checker_log"
artifact="$TMP/libfixture.a"
: > "$artifact"
(
  cd "$REPO"
  PATH="$BIN:$PATH" CALL_LOG="$checker_log" \
    scripts/check-macos-deployment-target.sh "$artifact" >/dev/null
)

for fixture in newer missing; do
  if (
    cd "$REPO"
    PATH="$BIN:$PATH" CALL_LOG="$checker_log" OTOOL_FIXTURE="$fixture" \
      scripts/check-macos-deployment-target.sh "$artifact" >/dev/null 2>&1
  ); then
    echo "deployment checker unexpectedly accepted $fixture archive" >&2
    exit 1
  fi
done

if (
  cd "$REPO"
  PATH="$BIN:$PATH" CALL_LOG="$checker_log" OTOOL_FIXTURE=exact-old \
    scripts/check-macos-deployment-target.sh --exact "$artifact" >/dev/null 2>&1
); then
  echo 'exact deployment checker unexpectedly accepted an older minimum' >&2
  exit 1
fi
(
  cd "$REPO"
  PATH="$BIN:$PATH" CALL_LOG="$checker_log" OTOOL_FIXTURE=exact \
    scripts/check-macos-deployment-target.sh --exact "$artifact" >/dev/null
)
echo 'ok - every archive member and exact final-image minimum are enforced'

# The deployment-target inspection does not authorize a later path lookup.
# Replace the sealed native inode after otool has observed it; the final
# XCFramework candidate must disagree with the original witness and refuse
# publication.
otool_swap_log="$TMP/otool-swap.log"
otool_swap_output="$TMP/otool-swap.out"
if OTOOL_INODE_SWAP=1 \
  run_script "$otool_swap_log" "$TMP/otool-swap-target" \
    --macos-only >"$otool_swap_output" 2>&1; then
  echo 'otool-time native inode replacement unexpectedly passed' >&2
  exit 1
fi
grep -Fq 'stored witness disagrees with a fresh structural witness' \
  "$otool_swap_output"
[[ ! -e $REPO/Packages/NMP/NMP.xcframework ]]
echo 'ok - otool-time native inode replacement cannot reach the XCFramework'

# Exercise a transient ABA at the final destination binding. The verifier
# captures the sealed staged inode before closing every directory descriptor,
# publishes it with no-replace semantics, then must reject any substituted
# final binding while preserving the restored exact bytes.
aba_hook="$TMP/swift-aba-hook"
aba_output="$TMP/swift-aba.out"
aba_log="$TMP/swift-aba.log"
aba_target="$TMP/swift-aba-target"
mkdir "$aba_hook"
NMP_COMPONENT_VERIFIER_HOOK_DIR="$aba_hook" \
  run_script "$aba_log" "$aba_target" --macos-only >"$aba_output" 2>&1 &
aba_pid=$!
wait_for_hook "$aba_pid" "$aba_hook/sources-pinned.ready" "$aba_output"
printf '1' >"$aba_hook/sources-pinned.release"
rm "$aba_hook/sources-pinned.ready"
wait_for_hook "$aba_pid" "$aba_hook/sources-pinned.ready" "$aba_output"
printf '1' >"$aba_hook/sources-pinned.release"
rm "$aba_hook/sources-pinned.ready"
# The wrapper re-validates every pinned source once more before it stages the
# publication, so release that barrier before waiting on the staged tree.
wait_for_hook "$aba_pid" "$aba_hook/sources-verified.ready" "$aba_output"
printf '1' >"$aba_hook/sources-verified.release"
wait_for_hook "$aba_pid" "$aba_hook/destination-staged.ready" "$aba_output"
printf '1' >"$aba_hook/destination-staged.release"
wait_for_hook "$aba_pid" "$aba_hook/destination-ready.ready" "$aba_output"
printf '1' >"$aba_hook/destination-ready.release"
wait_for_hook "$aba_pid" "$aba_hook/destination-published.ready" "$aba_output"
aba_destination="$REPO/Packages/NMP/NMP.xcframework"
mv "$aba_destination" "$aba_destination.verified"
mkdir -p "$aba_destination/macos-arm64"
printf '%s\n' 'attacker xcframework bytes' \
  >"$aba_destination/macos-arm64/libnmp_ffi.a"
mv "$aba_destination" "$aba_destination.attacker"
mv "$aba_destination.verified" "$aba_destination"
printf '1' >"$aba_hook/destination-published.release"
wait "$aba_pid"
[[ -f $aba_destination/macos-arm64/libnmp_ffi.a ]]
! grep -Fq 'attacker xcframework bytes' \
  "$aba_destination/macos-arm64/libnmp_ffi.a"
chmod -R u+w "$aba_destination.attacker"
rm -r "$aba_destination.attacker"
echo 'ok - final XCFramework ABA preserves the pinned staged bytes'
