#!/usr/bin/env bash
set -euo pipefail

SCRIPT=$(cd "$(dirname "$0")" && pwd)/build-kotlin-jvm.sh
COMPONENT_BUILDER=$(cd "$(dirname "$0")" && pwd)/build-component-release.sh
MANIFEST_VERIFIER=$(cd "$(dirname "$0")" && pwd)/verify-component-manifests.py
TMP=$(mktemp -d)
cleanup() {
  chmod -R u+w "$TMP" 2>/dev/null || true
  rm -r "$TMP"
}
trap cleanup EXIT

REPO="$TMP/repo"
BIN="$TMP/bin"
mkdir -p "$REPO/scripts" "$REPO/tools/component-artifact-witness" "$BIN"
REPO=$(cd "$REPO" && pwd -P)
BIN=$(cd "$BIN" && pwd -P)
cp "$SCRIPT" "$REPO/scripts/"
cp "$COMPONENT_BUILDER" "$REPO/scripts/"
cp "$MANIFEST_VERIFIER" "$REPO/scripts/"
touch "$REPO/tools/component-artifact-witness/Cargo.toml"
chmod +x "$REPO/scripts/"*.sh
git -C "$REPO" init -q

cat > "$BIN/uname" <<'SHIM'
#!/usr/bin/env bash
set -euo pipefail
case "${1:-}" in
  -s) printf '%s\n' Linux ;;
  -m) printf '%s\n' x86_64 ;;
  *) exit 64 ;;
esac
SHIM

cat > "$BIN/rustc" <<'SHIM'
#!/usr/bin/env bash
set -euo pipefail
case "${1:-}" in
  -vV)
    printf '%s\n' 'rustc 1.0.0 (fixture)' 'host: x86_64-unknown-linux-gnu'
    ;;
  *) exit 64 ;;
esac
SHIM

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
elif command == "witness":
    artifact = pathlib.Path(options["--artifact"])
    manifest_path = artifact.parent / "component-manifest.json"
    if manifest_path.is_file():
        manifest = json.loads(manifest_path.read_text())
    else:
        target = options["--target"]
        digest = lambda value: hashlib.sha256(value.encode()).hexdigest()
        identity = "nmp-core-component-v2-" + digest("nmp-core\0" + target)
        manifest = {
            "attestation_symbol": "NMP_CORE_COMPONENT_ATTESTATION_V2",
            "binding_identity": identity,
            "build_flags_digest": digest("fixture-flags"),
            "cargo_package": "nmp-ffi",
            "component_key": "nmp-core",
            "graph_digest": digest("fixture-graph-nmp-core"),
            "identity": identity,
            "interface_identity": (
                "nmp-component-interface-v2-" + digest("fixture-interface")
            ),
            "kind": "core",
            "library_stem": "nmp_ffi",
            "native_identity": identity,
            "profile": "release",
            "rustc_digest": digest("fixture-rustc"),
            "schema": 2,
            "target": target,
            "uniffi_namespace": "nmp_ffi",
        }
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
    attestation = {field: manifest[field] for field in attestation_fields}
    attestation["schema"] = 1
    callable_name = manifest["uniffi_namespace"] + "_fixture_call"
    print(canonical({
        "architecture": "x86_64",
        "artifact_blake3": hashlib.sha256(artifact.read_bytes()).hexdigest(),
        "artifact_size": artifact.stat().st_size,
        "attestation": attestation,
        "component_key": manifest["component_key"],
        "format": "elf-shared-object",
        "public_symbols": [callable_name],
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

cat > "$BIN/cargo" <<'SHIM'
#!/usr/bin/env bash
set -euo pipefail
printf 'cargo' >> "$CALL_LOG"
printf ' %q' "$@" >> "$CALL_LOG"
printf '\n' >> "$CALL_LOG"

target_dir=${CARGO_TARGET_DIR:-target}
if [[ $target_dir != /* ]]; then
  target_dir="$PWD/$target_dir"
fi

case "${1:-}" in
  fetch)
    ;;
  metadata)
    cat <<'JSON'
{"packages":[{"name":"nmp-ffi","metadata":{"nmp-component":{"bindgen-bin":"uniffi-bindgen","key":"nmp-core","kind":"core","library-stem":"nmp_ffi","schema":1,"uniffi-namespace":"nmp_ffi"}}}]}
JSON
    ;;
  build)
    if [[ " $* " == *" --manifest-path "*"component-artifact-witness/Cargo.toml"* ]]; then
      witness_target_dir=
      while [[ $# -gt 0 ]]; do
        case "$1" in
          --target-dir) witness_target_dir=$2; shift 2 ;;
          *) shift ;;
        esac
      done
      [[ -n $witness_target_dir ]]
      mkdir -p "$witness_target_dir/release"
      cp "$FIXTURE_WITNESS_TOOL" \
        "$witness_target_dir/release/nmp-component-artifact-witness"
      chmod +x "$witness_target_dir/release/nmp-component-artifact-witness"
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
    [[ $package == nmp-ffi && -n $target ]]
    release_dir="$target_dir/$target/release"
    mkdir -p "$release_dir"
    if [[ ${FAKE_OMIT_LIBRARY:-0} != 1 ]]; then
      printf '%s\n' "$target_dir" > "$release_dir/libnmp_ffi.so"
    fi
    python3 - "$NMP_COMPONENT_MANIFEST_OUTPUT" "$target" <<'PY'
import hashlib
import json
import pathlib
import sys

output, target = sys.argv[1:]
digest = lambda value: hashlib.sha256(value.encode()).hexdigest()
identity = "nmp-core-component-v2-" + digest("nmp-core\0" + target)
value = {
    "attestation_symbol": "NMP_CORE_COMPONENT_ATTESTATION_V2",
    "binding_identity": identity,
    "build_flags_digest": digest("fixture-flags"),
    "cargo_package": "nmp-ffi",
    "component_key": "nmp-core",
    "graph_digest": digest("fixture-graph-nmp-core"),
    "identity": identity,
    "interface_identity": "nmp-component-interface-v2-" + digest("fixture-interface"),
    "kind": "core",
    "library_stem": "nmp_ffi",
    "native_identity": identity,
    "profile": "release",
    "rustc_digest": digest("fixture-rustc"),
    "schema": 2,
    "target": target,
    "uniffi_namespace": "nmp_ffi",
}
pathlib.Path(output).write_text(
    json.dumps(value, separators=(",", ":"), sort_keys=True) + "\n"
)
PY
    if [[ ${FAKE_OMIT_BINDGEN:-0} != 1 ]]; then
      cat > "$release_dir/uniffi-bindgen" <<'BINDGEN'
#!/usr/bin/env bash
set -euo pipefail
printf 'bindgen %q' "$0" >> "$CALL_LOG"
printf ' %q' "$@" >> "$CALL_LOG"
printf '\n' >> "$CALL_LOG"
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
mkdir -p "$out_dir/uniffi/nmp_ffi"
printf 'generated from %s\n' "$library" > "$out_dir/uniffi/nmp_ffi/nmp_ffi.kt"
mkdir -p "$out_dir/uniffi/nmp_component_interface"
printf 'interface generated from %s\n' "$library" \
  > "$out_dir/uniffi/nmp_component_interface/nmp_component_interface.kt"
if [[ ${FAKE_BINDGEN_INODE_SWAP:-0} == 1 ]]; then
  snapshot_dir=$(dirname "$library")
  chmod u+w "$snapshot_dir"
  cp "$library" "$library.swap"
  chmod u+w "$library.swap"
  python3 - "$library.swap" <<'PY'
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
data = bytearray(path.read_bytes())
data[-1] = ord("X") if data[-1] != ord("X") else ord("Y")
path.write_bytes(data)
PY
  mv -f "$library.swap" "$library"
  chmod a-w "$library" "$snapshot_dir"
fi
BINDGEN
      chmod +x "$release_dir/uniffi-bindgen"
    fi
    ;;
  *) exit 64 ;;
esac
SHIM
chmod +x \
  "$BIN/uname" \
  "$BIN/rustc" \
  "$BIN/cargo" \
  "$BIN/component-artifact-witness-fixture"

run_script() {
  local log=$1 target_dir=${2:-}
  : > "$log"
  if [[ -n $target_dir ]]; then
    (
      cd "$REPO"
      if [[ -n ${NMP_COMPONENT_VERIFIER_HOOK_DIR:-} ]]; then
        export NMP_COMPONENT_VERIFIER_HOOK_DIR
      fi
      PATH="$BIN:$PATH" \
        CALL_LOG="$log" \
        CARGO_TARGET_DIR="$target_dir" \
        FIXTURE_WITNESS_TOOL="$BIN/component-artifact-witness-fixture" \
        scripts/build-kotlin-jvm.sh >/dev/null
    )
  else
    (
      cd "$REPO"
      if [[ -n ${NMP_COMPONENT_VERIFIER_HOOK_DIR:-} ]]; then
        export NMP_COMPONENT_VERIFIER_HOOK_DIR
      fi
      PATH="$BIN:$PATH" \
        CALL_LOG="$log" \
        FIXTURE_WITNESS_TOOL="$BIN/component-artifact-witness-fixture" \
        env -u CARGO_TARGET_DIR scripts/build-kotlin-jvm.sh >/dev/null
    )
  fi
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

run_failure() {
  local log=$1 output=$2 target_dir=$3 omission=$4
  : > "$log"
  (
    cd "$REPO"
    PATH="$BIN:$PATH" \
      CALL_LOG="$log" \
      CARGO_TARGET_DIR="$target_dir" \
      FIXTURE_WITNESS_TOOL="$BIN/component-artifact-witness-fixture" \
      env "$omission=1" scripts/build-kotlin-jvm.sh >"$output" 2>&1
  )
}

assert_single_plan() {
  local log=$1
  [[ $(grep -c '^cargo fetch ' "$log") -eq 1 ]]
  [[ $(grep -c '^cargo build --frozen -p nmp-ffi ' "$log") -eq 1 ]]
  [[ $(grep -c '^cargo build --manifest-path ' "$log") -eq 1 ]]
  [[ $(grep -c '^bindgen ' "$log") -eq 1 ]]
  ! grep -q '^cargo run ' "$log"
}

assert_contains() {
  local expected=$1 file=$2
  grep -Fq -- "$expected" "$file" || {
    echo "missing expected text: $expected" >&2
    cat "$file" >&2
    exit 1
  }
}

assert_outputs() {
  local target_dir=$1
  local generated="$REPO/Packages/NMPKotlin/src/main/kotlin/uniffi/nmp_ffi/nmp_ffi.kt"
  local resource="$REPO/Packages/NMPKotlin/src/main/resources/linux-x86-64/libnmp_ffi.so"
  assert_contains \
    "generated from $target_dir/nmp-component-artifacts-v2/nmp-core/$HOST_TARGET." \
    "$generated"
  assert_contains "$target_dir/nmp-component-build-v2/nmp-core" "$resource"
}

reset_fixture_outputs() {
  local path
  for path in "$@"; do
    if [[ -e $path ]]; then
      chmod -R u+w "$path" 2>/dev/null || true
      rm -r "$path"
    fi
  done
}

HOST_TARGET=$(PATH="$BIN:$PATH" rustc -vV | sed -n 's/^host: //p')
[[ -n "$HOST_TARGET" ]]

# With no override, preserve the historical repository-local target directory.
default_log="$TMP/default.log"
run_script "$default_log"
assert_single_plan "$default_log"
assert_contains "$REPO/target/nmp-component-artifacts-v2/nmp-core/$HOST_TARGET." "$default_log"
assert_outputs "$REPO/target"
echo 'ok - default target directory remains repository-local'

# An absolute shared cache supplies both the release library and bindgen's
# Cargo target. No lookup or fallback build may touch the repository target.
absolute_log="$TMP/absolute.log"
absolute_target="$TMP/shared-cache"
reset_fixture_outputs "$REPO/target" "$REPO/Packages" "$REPO/gen-kotlin"
run_script "$absolute_log" "$absolute_target"
assert_single_plan "$absolute_log"
assert_contains "$absolute_target/nmp-component-artifacts-v2/nmp-core/$HOST_TARGET." "$absolute_log"
! grep -Fq "$REPO/target/release" "$absolute_log"
[[ ! -e $REPO/target ]]
assert_outputs "$absolute_target"
echo 'ok - absolute CARGO_TARGET_DIR is used without fallback or duplicate build'

# Cargo resolves a relative override from the repository root because that is
# the working directory for both invocations. Artifact lookup must match.
relative_log="$TMP/relative.log"
relative_value=relative-cache
relative_target="$REPO/$relative_value"
reset_fixture_outputs \
  "$REPO/target" "$REPO/Packages" "$REPO/gen-kotlin" "$relative_target"
run_script "$relative_log" "$relative_value"
assert_single_plan "$relative_log"
assert_contains "$relative_target/nmp-component-artifacts-v2/nmp-core/$HOST_TARGET." "$relative_log"
! grep -Fq "$REPO/target/release" "$relative_log"
[[ ! -e $REPO/target ]]
assert_outputs "$relative_target"
echo 'ok - relative CARGO_TARGET_DIR lookup matches Cargo resolution'

# A successful Cargo exit without either required release artifact must fail
# before generation and name the exact resolved path that is missing.
missing_library_log="$TMP/missing-library.log"
missing_library_output="$TMP/missing-library.out"
missing_library_target="$TMP/missing-library-target"
if run_failure \
  "$missing_library_log" "$missing_library_output" \
  "$missing_library_target" FAKE_OMIT_LIBRARY; then
  echo 'missing release library unexpectedly passed' >&2
  exit 1
fi
assert_contains \
  "component-build: expected libnmp_ffi under $missing_library_target/nmp-component-build-v2/nmp-core/$HOST_TARGET/release" \
  "$missing_library_output"
[[ $(grep -c '^cargo build --frozen -p nmp-ffi ' "$missing_library_log") -eq 1 ]]
! grep -q '^bindgen ' "$missing_library_log"

missing_bindgen_log="$TMP/missing-bindgen.log"
missing_bindgen_output="$TMP/missing-bindgen.out"
missing_bindgen_target="$TMP/missing-bindgen-target"
if run_failure \
  "$missing_bindgen_log" "$missing_bindgen_output" \
  "$missing_bindgen_target" FAKE_OMIT_BINDGEN; then
  echo 'missing release bindgen unexpectedly passed' >&2
  exit 1
fi
assert_contains "error: expected executable" "$missing_bindgen_output"
assert_contains \
  "$missing_bindgen_target/nmp-component-artifacts-v2/nmp-core/$HOST_TARGET." \
  "$missing_bindgen_output"
assert_contains \
  "/uniffi-bindgen in the sealed component snapshot" \
  "$missing_bindgen_output"
[[ $(grep -c '^cargo build --frozen -p nmp-ffi ' "$missing_bindgen_log") -eq 1 ]]
! grep -q '^bindgen ' "$missing_bindgen_log"
echo 'ok - missing release artifacts fail early with resolved-path diagnostics'

# Bindgen is allowed to read the sealed native library but cannot replace the
# inode and make the later JNA copy authoritative. The final candidate is
# checked against the original witness before resources are published.
bindgen_swap_log="$TMP/bindgen-swap.log"
bindgen_swap_output="$TMP/bindgen-swap.out"
bindgen_swap_target="$TMP/bindgen-swap-target"
reset_fixture_outputs "$REPO/Packages" "$REPO/gen-kotlin"
if run_failure \
  "$bindgen_swap_log" "$bindgen_swap_output" \
  "$bindgen_swap_target" FAKE_BINDGEN_INODE_SWAP; then
  echo 'bindgen-time native inode replacement unexpectedly passed' >&2
  exit 1
fi
assert_contains 'stored witness disagrees with a fresh structural witness' \
  "$bindgen_swap_output"
[[ ! -e $REPO/Packages/NMPKotlin/src/main/resources ]]
echo 'ok - bindgen-time native inode replacement cannot reach JNA resources'

# The final publication name is also authority. Replace the just-published
# resource directory after the verifier atomically installs its captured
# staged inode; the wrapper must fail instead of accepting the
# attacker-controlled binding.
publish_hook="$TMP/kotlin-publish-hook"
publish_output="$TMP/kotlin-publish.out"
publish_log="$TMP/kotlin-publish.log"
publish_target="$TMP/kotlin-publish-target"
mkdir "$publish_hook"
reset_fixture_outputs "$REPO/Packages" "$REPO/gen-kotlin"
NMP_COMPONENT_VERIFIER_HOOK_DIR="$publish_hook" \
  run_script "$publish_log" "$publish_target" >"$publish_output" 2>&1 &
publish_pid=$!
wait_for_hook \
  "$publish_pid" "$publish_hook/sources-pinned.ready" "$publish_output"
printf '1' >"$publish_hook/sources-pinned.release"
rm "$publish_hook/sources-pinned.ready"
# The first source-pin phase belongs to the managed component builder. The
# second is the wrapper's final candidate-tree verification.
wait_for_hook \
  "$publish_pid" "$publish_hook/sources-pinned.ready" "$publish_output"
printf '1' >"$publish_hook/sources-pinned.release"
rm "$publish_hook/sources-pinned.ready"
# The wrapper re-validates every pinned source once more before it stages the
# publication, so release that barrier before waiting on the staged tree.
wait_for_hook \
  "$publish_pid" "$publish_hook/sources-verified.ready" "$publish_output"
printf '1' >"$publish_hook/sources-verified.release"
wait_for_hook \
  "$publish_pid" "$publish_hook/destination-staged.ready" "$publish_output"
printf '1' >"$publish_hook/destination-staged.release"
wait_for_hook \
  "$publish_pid" "$publish_hook/destination-ready.ready" "$publish_output"
printf '1' >"$publish_hook/destination-ready.release"
wait_for_hook \
  "$publish_pid" "$publish_hook/destination-published.ready" "$publish_output"
published_resources="$REPO/Packages/NMPKotlin/src/main/resources"
mv "$published_resources" "$published_resources.verified"
mkdir -p "$published_resources/linux-x86-64"
printf '%s\n' 'attacker native bytes' \
  >"$published_resources/linux-x86-64/libnmp_ffi.so"
chmod -R a-w "$published_resources"
printf '1' >"$publish_hook/destination-published.release"
if wait "$publish_pid"; then
  echo 'final JNA resource directory replacement unexpectedly passed' >&2
  exit 1
fi
assert_contains 'published binding is not the staged directory inode' \
  "$publish_output"
assert_contains 'attacker native bytes' \
  "$published_resources/linux-x86-64/libnmp_ffi.so"
reset_fixture_outputs "$published_resources" "$published_resources.verified"
if find "$REPO" -name '*.aar' -print -quit | grep -q .; then
  echo 'Kotlin/JVM fixture unexpectedly assembled an Android AAR' >&2
  exit 1
fi
echo 'ok - final JNA tree substitution is refused without assembling an AAR'
