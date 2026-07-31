#!/usr/bin/env bash

set -euo pipefail

ROOT=$(git rev-parse --show-toplevel)
cd "$ROOT"
TMP=$(mktemp -d "${TMPDIR:-/tmp}/nmp-component-manifests.XXXXXX")
ACTIVE_VERIFIER_PID=
cleanup() {
  if [[ -n $ACTIVE_VERIFIER_PID ]] &&
    kill -0 "$ACTIVE_VERIFIER_PID" 2>/dev/null; then
    kill "$ACTIVE_VERIFIER_PID" 2>/dev/null || true
    wait "$ACTIVE_VERIFIER_PID" 2>/dev/null || true
  fi
  chmod -R u+w "$TMP" 2>/dev/null || true
  rm -r "$TMP"
}
trap cleanup EXIT

python3 - "$TMP" <<'PY'
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
digest = "1" * 64
interface = "nmp-component-interface-v2-" + "2" * 64
core_identity = "nmp-core-component-v2-" + "3" * 64
common = {
    "attestation_symbol": "NMP_CORE_COMPONENT_ATTESTATION_V2",
    "binding_identity": core_identity,
    "build_flags_digest": digest,
    "cargo_package": "nmp-ffi",
    "component_key": "nmp-core",
    "graph_digest": "4" * 64,
    "identity": core_identity,
    "interface_identity": interface,
    "kind": "core",
    "library_stem": "nmp_ffi",
    "native_identity": core_identity,
    "profile": "release",
    "rustc_digest": "5" * 64,
    "schema": 2,
    "target": "aarch64-apple-darwin",
    "uniffi_namespace": "nmp_ffi",
}
provider_identity = "nmp-nip46-component-v2-" + "6" * 64
provider = {
    **common,
    "attestation_symbol": "NMP_NIP46_COMPONENT_ATTESTATION_V2",
    "binding_identity": provider_identity,
    "cargo_package": "nmp-nip46-ffi",
    "component_key": "nmp-nip46",
    "graph_digest": "7" * 64,
    "identity": provider_identity,
    "kind": "optional",
    "library_stem": "nmp_nip46_ffi",
    "native_identity": provider_identity,
    "required_core_artifact_blake3": "8" * 64,
    "required_core_identity": core_identity,
    "required_core_manifest_blake3": "9" * 64,
    "uniffi_namespace": "nmp_nip46_ffi",
}
for name, value in (("core", common), ("provider", provider)):
    (root / f"{name}.json").write_text(
        json.dumps(value, separators=(",", ":"), sort_keys=True) + "\n"
    )
PY

scripts/verify-component-manifests.py "$TMP/provider.json" "$TMP/core.json" >"$TMP/one"
scripts/verify-component-manifests.py "$TMP/core.json" "$TMP/provider.json" >"$TMP/two"
cmp "$TMP/one" "$TMP/two"

mutate_and_refuse() {
  local name=$1
  local expression=$2
  local expected=$3
  python3 - "$TMP/provider.json" "$TMP/$name.json" "$expression" <<'PY'
import json, pathlib, sys
value = json.loads(pathlib.Path(sys.argv[1]).read_text())
exec(sys.argv[3], {}, {"value": value})
pathlib.Path(sys.argv[2]).write_text(
    json.dumps(value, separators=(",", ":"), sort_keys=True) + "\n"
)
PY
  if scripts/verify-component-manifests.py "$TMP/core.json" "$TMP/$name.json" \
    >"$TMP/$name.out" 2>&1; then
    echo "component-manifests-test: mutation $name passed" >&2
    exit 1
  fi
  grep -qF "$expected" "$TMP/$name.out"
}

mutate_and_refuse binding \
  'value["binding_identity"]="nmp-nip46-component-v2-"+"8"*64' \
  'binding identity does not equal'
mutate_and_refuse native \
  'value["native_identity"]="nmp-nip46-component-v2-"+"8"*64' \
  'native identity does not equal'
mutate_and_refuse interface \
  'value["interface_identity"]="nmp-component-interface-v2-"+"8"*64' \
  'interface_identity disagrees'
mutate_and_refuse required-core \
  'value["required_core_identity"]="nmp-core-component-v2-"+"8"*64' \
  'required_core_identity'
mutate_and_refuse flags \
  'value["build_flags_digest"]="8"*64' \
  'build_flags_digest disagrees'
mutate_and_refuse compiler \
  'value["rustc_digest"]="8"*64' \
  'rustc_digest disagrees'

python3 - "$TMP/provider.json" "$TMP/duplicate.json" <<'PY'
import json, pathlib, sys
value = json.loads(pathlib.Path(sys.argv[1]).read_text())
value["identity"] = value["binding_identity"] = value["native_identity"] = (
    "nmp-nip46-component-v2-" + "9" * 64
)
pathlib.Path(sys.argv[2]).write_text(
    json.dumps(value, separators=(",", ":"), sort_keys=True) + "\n"
)
PY
if scripts/verify-component-manifests.py \
  "$TMP/core.json" "$TMP/provider.json" "$TMP/duplicate.json" \
  >"$TMP/duplicate.out" 2>&1; then
  echo "component-manifests-test: duplicate stable key passed" >&2
  exit 1
fi
grep -qF 'duplicate component_key' "$TMP/duplicate.out"

PUBLICATION_TEMPLATE="$TMP/publication-template"
python3 - "$PUBLICATION_TEMPLATE" <<'PY'
import hashlib
import json
import pathlib
import stat
import sys

root = pathlib.Path(sys.argv[1])
core = root / "core-authority"
source = root / "provider-source"
core.mkdir(parents=True)
(source / "jna" / "linux-x86-64").mkdir(parents=True)
(source / "NMPNip46.xcframework" / "macos-arm64").mkdir(parents=True)

canonical = lambda value: (
    json.dumps(value, separators=(",", ":"), sort_keys=True) + "\n"
)
digest = lambda value: hashlib.sha256(value).hexdigest()
interface = "nmp-component-interface-v2-" + "2" * 64
core_identity = "nmp-core-component-v2-" + "3" * 64
provider_identity = "nmp-nip46-component-v2-" + "6" * 64
common = {
    "attestation_symbol": "NMP_CORE_COMPONENT_ATTESTATION_V2",
    "binding_identity": core_identity,
    "build_flags_digest": "1" * 64,
    "cargo_package": "nmp-ffi",
    "component_key": "nmp-core",
    "graph_digest": "4" * 64,
    "identity": core_identity,
    "interface_identity": interface,
    "kind": "core",
    "library_stem": "nmp_ffi",
    "native_identity": core_identity,
    "profile": "release",
    "rustc_digest": "5" * 64,
    "schema": 2,
    "target": "aarch64-apple-darwin",
    "uniffi_namespace": "nmp_ffi",
}
core_artifact = core / "libnmp_ffi.a"
core_manifest = core / "component-manifest.json"
core_artifact.write_bytes(b"pinned core artifact bytes\n")
core_manifest.write_text(canonical(common))
provider = {
    **common,
    "attestation_symbol": "NMP_NIP46_COMPONENT_ATTESTATION_V2",
    "binding_identity": provider_identity,
    "cargo_package": "nmp-nip46-ffi",
    "component_key": "nmp-nip46",
    "graph_digest": "7" * 64,
    "identity": provider_identity,
    "kind": "optional",
    "library_stem": "nmp_nip46_ffi",
    "native_identity": provider_identity,
    "required_core_artifact_blake3": digest(core_artifact.read_bytes()),
    "required_core_identity": core_identity,
    "required_core_manifest_blake3": digest(core_manifest.read_bytes()),
    "uniffi_namespace": "nmp_nip46_ffi",
}
provider_artifact = source / "libnmp_nip46_ffi.a"
provider_manifest = source / "component-manifest.json"
provider_artifact.write_bytes(b"pinned provider artifact bytes\n")
provider_manifest.write_text(canonical(provider))

def witness(manifest, artifact):
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
    return {
        "architecture": "aarch64",
        "artifact_blake3": digest(artifact.read_bytes()),
        "artifact_size": artifact.stat().st_size,
        "attestation": attestation,
        "component_key": manifest["component_key"],
        "format": "archive-macho",
        "public_symbols": ["_" + callable_name],
        "schema": 1,
        "target": manifest["target"],
        "uniffi_components": [{
            "callables": [callable_name],
            "namespace": manifest["uniffi_namespace"],
        }],
    }

(core / "libnmp_ffi.a.witness.json").write_text(
    canonical(witness(common, core_artifact))
)
(source / "libnmp_nip46_ffi.a.witness.json").write_text(
    canonical(witness(provider, provider_artifact))
)
symbols = b"nmp_component_interface_fixture\0"
(source / "component-interface-forbidden-symbols.nul").write_bytes(symbols)
(source / "component-interface-localization-plan.json").write_text(canonical({
    "artifact_blake3": digest(core_artifact.read_bytes()),
    "interface_namespace": "nmp_component_interface",
    "schema": 1,
    "symbols": ["nmp_component_interface_fixture"],
}))
(source / "jna" / "linux-x86-64" / "provider.payload").write_bytes(
    b"exact JNA payload bytes\x00\xff\n"
)
(source / "NMPNip46.xcframework" / "Info.plist").write_bytes(
    b"<?xml version=\"1.0\"?><plist><dict/></plist>\n"
)
(source / "NMPNip46.xcframework" / "macos-arm64" /
 "provider.payload").write_bytes(b"exact XCFramework payload bytes\n")

audit = source / "nmp-nip46-metadata-audit"
audit.write_text("#!/bin/sh\nset -eu\n[ -r \"$1\" ]\n")
audit.chmod(0o555)

tool = root / "component-artifact-witness"
tool.write_text(r'''#!/usr/bin/env python3
import hashlib
import json
import pathlib
import sys

canonical = lambda value: json.dumps(
    value, separators=(",", ":"), sort_keys=True
) + "\n"
digest = lambda value: hashlib.sha256(value).hexdigest()
interface = "nmp-component-interface-v2-" + "2" * 64
core_identity = "nmp-core-component-v2-" + "3" * 64
provider_identity = "nmp-nip46-component-v2-" + "6" * 64
core_artifact_bytes = b"pinned core artifact bytes\n"
common = {
    "attestation_symbol": "NMP_CORE_COMPONENT_ATTESTATION_V2",
    "binding_identity": core_identity,
    "build_flags_digest": "1" * 64,
    "cargo_package": "nmp-ffi",
    "component_key": "nmp-core",
    "graph_digest": "4" * 64,
    "identity": core_identity,
    "interface_identity": interface,
    "kind": "core",
    "library_stem": "nmp_ffi",
    "native_identity": core_identity,
    "profile": "release",
    "rustc_digest": "5" * 64,
    "schema": 2,
    "target": "aarch64-apple-darwin",
    "uniffi_namespace": "nmp_ffi",
}
core_manifest_bytes = canonical(common).encode()
provider = {
    **common,
    "attestation_symbol": "NMP_NIP46_COMPONENT_ATTESTATION_V2",
    "binding_identity": provider_identity,
    "cargo_package": "nmp-nip46-ffi",
    "component_key": "nmp-nip46",
    "graph_digest": "7" * 64,
    "identity": provider_identity,
    "kind": "optional",
    "library_stem": "nmp_nip46_ffi",
    "native_identity": provider_identity,
    "required_core_artifact_blake3": digest(core_artifact_bytes),
    "required_core_identity": core_identity,
    "required_core_manifest_blake3": digest(core_manifest_bytes),
    "uniffi_namespace": "nmp_nip46_ffi",
}

args = sys.argv[2:]
options = {}
while args:
    option = args.pop(0)
    options[option] = args.pop(0)
command = sys.argv[1]
if command == "digest":
    print(digest(pathlib.Path(options["--file"]).read_bytes()))
elif command == "plan-localization":
    artifact = pathlib.Path(options["--artifact"])
    pathlib.Path(options["--out"]).write_bytes(
        b"nmp_component_interface_fixture\0"
    )
    print(canonical({
        "artifact_blake3": digest(artifact.read_bytes()),
        "interface_namespace": options["--interface-namespace"],
        "schema": 1,
        "symbols": ["nmp_component_interface_fixture"],
    }), end="")
elif command == "witness":
    artifact = pathlib.Path(options["--artifact"])
    manifest = (
        common if options["--component-key"] == "nmp-core" else provider
    )
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
    print(canonical({
        "architecture": "aarch64",
        "artifact_blake3": digest(artifact.read_bytes()),
        "artifact_size": artifact.stat().st_size,
        "attestation": attestation,
        "component_key": manifest["component_key"],
        "format": "archive-macho",
        "public_symbols": ["_" + callable_name],
        "schema": 1,
        "target": manifest["target"],
        "uniffi_components": [{
            "callables": [callable_name],
            "namespace": manifest["uniffi_namespace"],
        }],
    }), end="")
else:
    raise SystemExit(64)
''')
tool.chmod(0o555)

for path in sorted(root.rglob("*"), reverse=True):
    mode = stat.S_IMODE(path.stat().st_mode)
    path.chmod(mode & ~0o222)
root.chmod(0o555)
PY

publish_fixture() {
  local fixture=$1
  local destination=$2
  scripts/verify-component-manifests.py \
    --witness-tool "$fixture/component-artifact-witness" \
    --artifact "$fixture/core-authority/libnmp_ffi.a" \
    "$fixture/core-authority/component-manifest.json" \
    --witness "$fixture/core-authority/libnmp_ffi.a.witness.json" \
    --artifact "$fixture/provider-source/libnmp_nip46_ffi.a" \
    "$fixture/provider-source/component-manifest.json" \
    --witness "$fixture/provider-source/libnmp_nip46_ffi.a.witness.json" \
    --metadata-audit "$fixture/provider-source/nmp-nip46-metadata-audit" \
    --forbid-symbols \
    "$fixture/provider-source/component-interface-forbidden-symbols.nul" \
    --localization-source "$fixture/core-authority/libnmp_ffi.a" \
    --localization-plan \
    "$fixture/provider-source/component-interface-localization-plan.json" \
    --publish-payload \
    --publish-tree "$fixture/provider-source" "$destination"
}

fresh_publication_fixture() {
  local name=$1
  local fixture="$TMP/$name"
  cp -R "$PUBLICATION_TEMPLATE" "$fixture"
  printf '%s\n' "$fixture"
}

wait_for_ready() {
  local pid=$1
  local ready=$2
  local output=$3
  local deadline=$((SECONDS + 15))
  while [[ ! -e $ready ]]; do
    if ! kill -0 "$pid" 2>/dev/null; then
      wait "$pid" || true
      echo "component-manifests-test: verifier exited before $ready" >&2
      sed -n '1,160p' "$output" >&2
      exit 1
    fi
    if (( SECONDS >= deadline )); then
      kill "$pid" 2>/dev/null || true
      wait "$pid" || true
      echo "component-manifests-test: timed out waiting for $ready" >&2
      exit 1
    fi
  done
}

wait_for_child() {
  local pid=$1
  local output=$2
  local deadline=$((SECONDS + 15))
  while kill -0 "$pid" 2>/dev/null; do
    if (( SECONDS >= deadline )); then
      kill "$pid" 2>/dev/null || true
      wait "$pid" || true
      echo "component-manifests-test: verifier did not finish" >&2
      sed -n '1,160p' "$output" >&2
      exit 1
    fi
  done
  CHILD_STATUS=0
  wait "$pid" || CHILD_STATUS=$?
}

release_if_reached() {
  local pid=$1
  local hook=$2
  local phase=$3
  local output=$4
  local deadline=$((SECONDS + 15))
  while [[ ! -e $hook/$phase.ready ]]; do
    if ! kill -0 "$pid" 2>/dev/null; then
      return
    fi
    if (( SECONDS >= deadline )); then
      kill "$pid" 2>/dev/null || true
      wait "$pid" || true
      echo "component-manifests-test: timed out awaiting optional $phase hook" >&2
      sed -n '1,160p' "$output" >&2
      exit 1
    fi
  done
  printf '1' >"$hook/$phase.release"
}

assert_trees_equal() {
  python3 - "$1" "$2" <<'PY'
import pathlib
import stat
import sys

left = pathlib.Path(sys.argv[1])
right = pathlib.Path(sys.argv[2])
left_entries = {
    path.relative_to(left): path
    for path in left.rglob("*")
}
right_entries = {
    path.relative_to(right): path
    for path in right.rglob("*")
}
if set(left_entries) != set(right_entries):
    raise SystemExit(
        f"published path set disagrees: "
        f"{sorted(set(left_entries) ^ set(right_entries))}"
    )
for relative, source in left_entries.items():
    published = right_entries[relative]
    if source.is_dir() != published.is_dir():
        raise SystemExit(f"published entry type disagrees: {relative}")
    if source.is_file():
        if source.read_bytes() != published.read_bytes():
            raise SystemExit(f"published bytes disagree: {relative}")
        if stat.S_IMODE(published.stat().st_mode) & 0o222:
            raise SystemExit(f"published file is writable: {relative}")
    elif stat.S_IMODE(published.stat().st_mode) & 0o222:
        raise SystemExit(f"published directory is writable: {relative}")
PY
}

expect_publish_refusal() {
  local name=$1
  local fixture=$2
  local destination=$3
  local expected=$4
  if publish_fixture "$fixture" "$destination" >"$TMP/$name.out" 2>&1; then
    echo "component-manifests-test: publication mutation $name passed" >&2
    exit 1
  fi
  grep -qF "$expected" "$TMP/$name.out"
}

run_barrier_mutation() {
  local name=$1
  local phase=$2
  local mutation=$3
  local expected_status=$4
  local expected_message=$5
  local fixture
  fixture=$(fresh_publication_fixture "$name")
  local hook="$TMP/$name-hooks"
  local destination_parent="$TMP/$name-destination-parent"
  local destination="$destination_parent/final"
  mkdir "$hook" "$destination_parent"
  NMP_COMPONENT_VERIFIER_HOOK_DIR="$hook" \
    publish_fixture "$fixture" "$destination" >"$TMP/$name.out" 2>&1 &
  local pid=$!
  ACTIVE_VERIFIER_PID=$pid
  if [[ $phase != sources-pinned ]]; then
    wait_for_ready "$pid" "$hook/sources-pinned.ready" "$TMP/$name.out"
    printf '1' >"$hook/sources-pinned.release"
  fi
  if [[ $phase == destination-published ]]; then
    wait_for_ready "$pid" "$hook/destination-staged.ready" "$TMP/$name.out"
    printf '1' >"$hook/destination-staged.release"
  fi
  wait_for_ready "$pid" "$hook/$phase.ready" "$TMP/$name.out"

  case "$mutation" in
    ancestor-directory)
      chmod u+w "$fixture"
      mv "$fixture/provider-source" "$fixture/provider-source.original"
      cp -R "$fixture/provider-source.original" "$fixture/provider-source"
      ;;
    artifact-inode)
      chmod u+w "$fixture/provider-source"
      mv "$fixture/provider-source/libnmp_nip46_ffi.a" \
        "$fixture/provider-source/libnmp_nip46_ffi.a.original"
      cp "$fixture/provider-source/libnmp_nip46_ffi.a.original" \
        "$fixture/provider-source/libnmp_nip46_ffi.a"
      chmod a-w "$fixture/provider-source/libnmp_nip46_ffi.a"
      ;;
    manifest-inode)
      chmod u+w "$fixture/provider-source"
      mv "$fixture/provider-source/component-manifest.json" \
        "$fixture/provider-source/component-manifest.json.original"
      cp "$fixture/provider-source/component-manifest.json.original" \
        "$fixture/provider-source/component-manifest.json"
      chmod a-w "$fixture/provider-source/component-manifest.json"
      ;;
    witness-inode)
      chmod u+w "$fixture/provider-source"
      mv "$fixture/provider-source/libnmp_nip46_ffi.a.witness.json" \
        "$fixture/provider-source/libnmp_nip46_ffi.a.witness.json.original"
      cp "$fixture/provider-source/libnmp_nip46_ffi.a.witness.json.original" \
        "$fixture/provider-source/libnmp_nip46_ffi.a.witness.json"
      chmod a-w "$fixture/provider-source/libnmp_nip46_ffi.a.witness.json"
      ;;
    forbidden-inode)
      chmod u+w "$fixture/provider-source"
      mv "$fixture/provider-source/component-interface-forbidden-symbols.nul" \
        "$fixture/provider-source/component-interface-forbidden-symbols.nul.original"
      cp "$fixture/provider-source/component-interface-forbidden-symbols.nul.original" \
        "$fixture/provider-source/component-interface-forbidden-symbols.nul"
      chmod a-w \
        "$fixture/provider-source/component-interface-forbidden-symbols.nul"
      ;;
    plan-inode)
      chmod u+w "$fixture/provider-source"
      mv "$fixture/provider-source/component-interface-localization-plan.json" \
        "$fixture/provider-source/component-interface-localization-plan.json.original"
      cp "$fixture/provider-source/component-interface-localization-plan.json.original" \
        "$fixture/provider-source/component-interface-localization-plan.json"
      chmod a-w \
        "$fixture/provider-source/component-interface-localization-plan.json"
      ;;
    audit-inode)
      chmod u+w "$fixture/provider-source"
      mv "$fixture/provider-source/nmp-nip46-metadata-audit" \
        "$fixture/provider-source/nmp-nip46-metadata-audit.original"
      cp "$fixture/provider-source/nmp-nip46-metadata-audit.original" \
        "$fixture/provider-source/nmp-nip46-metadata-audit"
      chmod 0555 "$fixture/provider-source/nmp-nip46-metadata-audit"
      ;;
    aba-restore)
      chmod u+w "$fixture/provider-source"
      mv "$fixture/provider-source/libnmp_nip46_ffi.a" \
        "$fixture/provider-source/libnmp_nip46_ffi.a.original"
      printf '%s\n' 'attacker replacement bytes' \
        >"$fixture/provider-source/libnmp_nip46_ffi.a"
      rm "$fixture/provider-source/libnmp_nip46_ffi.a"
      mv "$fixture/provider-source/libnmp_nip46_ffi.a.original" \
        "$fixture/provider-source/libnmp_nip46_ffi.a"
      chmod a-w "$fixture/provider-source/libnmp_nip46_ffi.a"
      ;;
    destination-parent)
      chmod u+w "$fixture"
      mv "$destination_parent" "$fixture/destination-parent.original"
      mkdir "$destination_parent"
      ;;
    final-name)
      printf '%s\n' 'must remain untouched' >"$TMP/$name-final-name-target"
      ln -s "$TMP/$name-final-name-target" "$destination"
      ;;
    published-name-inode)
      mv "$destination" "$destination.original"
      cp -R "$destination.original" "$destination"
      ;;
    published-name-symlink)
      mv "$destination" "$destination.original"
      ln -s "$destination.original" "$destination"
      ;;
    *)
      echo "component-manifests-test: unknown mutation $mutation" >&2
      kill "$pid" 2>/dev/null || true
      exit 1
      ;;
  esac

  printf '1' >"$hook/$phase.release"
  if [[ $phase == sources-pinned ]]; then
    release_if_reached "$pid" "$hook" destination-staged "$TMP/$name.out"
    release_if_reached "$pid" "$hook" destination-published "$TMP/$name.out"
  elif [[ $phase == destination-staged ]]; then
    release_if_reached "$pid" "$hook" destination-published "$TMP/$name.out"
  fi
  wait_for_child "$pid" "$TMP/$name.out"
  ACTIVE_VERIFIER_PID=
  local status=$CHILD_STATUS
  if [[ $expected_status == success ]]; then
    if [[ $status != 0 ]]; then
      echo "component-manifests-test: ABA publication was refused" >&2
      sed -n '1,160p' "$TMP/$name.out" >&2
      exit 1
    fi
    assert_trees_equal "$fixture/provider-source" "$destination"
  else
    if [[ $status == 0 ]]; then
      echo "component-manifests-test: mutation $name was accepted" >&2
      exit 1
    fi
    grep -qF "$expected_message" "$TMP/$name.out"
  fi
  if [[ $mutation == final-name ]]; then
    [[ -L $destination ]]
    grep -qF 'must remain untouched' "$TMP/$name-final-name-target"
  fi
  if [[ $mutation == published-name-symlink ]]; then
    [[ -L $destination ]]
  fi
}

positive_fixture=$(fresh_publication_fixture publication-positive)
positive_destination="$TMP/publication-positive-destination"
mkdir "$positive_destination"
publish_fixture "$positive_fixture" \
  "$positive_destination/final" >/dev/null
assert_trees_equal "$positive_fixture/provider-source" \
  "$positive_destination/final"

symlink_fixture=$(fresh_publication_fixture publication-leaf-symlink)
chmod u+w \
  "$symlink_fixture/provider-source/NMPNip46.xcframework/macos-arm64"
mv \
  "$symlink_fixture/provider-source/NMPNip46.xcframework/macos-arm64/provider.payload" \
  "$symlink_fixture/provider-source/NMPNip46.xcframework/macos-arm64/provider.payload.original"
ln -s 'provider.payload.original' \
  "$symlink_fixture/provider-source/NMPNip46.xcframework/macos-arm64/provider.payload"
chmod a-w \
  "$symlink_fixture/provider-source/NMPNip46.xcframework/macos-arm64"
symlink_destination="$TMP/publication-leaf-symlink-destination"
mkdir "$symlink_destination"
expect_publish_refusal publication-leaf-symlink "$symlink_fixture" \
  "$symlink_destination/final" \
  'publish tree entries must be regular files or directories'

run_barrier_mutation publication-ancestor sources-pinned \
  ancestor-directory refusal 'pinned directory binding changed'
run_barrier_mutation publication-artifact-inode sources-pinned \
  artifact-inode refusal 'pinned path identity changed'
run_barrier_mutation publication-manifest-inode sources-pinned \
  manifest-inode refusal 'pinned path identity changed'
run_barrier_mutation publication-witness-inode sources-pinned \
  witness-inode refusal 'pinned path identity changed'
run_barrier_mutation publication-forbidden-inode sources-pinned \
  forbidden-inode refusal 'pinned path identity changed'
run_barrier_mutation publication-plan-inode sources-pinned \
  plan-inode refusal 'pinned path identity changed'
run_barrier_mutation publication-audit-inode sources-pinned \
  audit-inode refusal 'pinned path identity changed'
run_barrier_mutation publication-aba sources-pinned \
  aba-restore success ''
run_barrier_mutation publication-destination-parent destination-staged \
  destination-parent refusal 'pinned directory binding changed'
run_barrier_mutation publication-final-name destination-staged \
  final-name refusal 'final publication binding appeared during staging'
run_barrier_mutation publication-published-inode destination-published \
  published-name-inode refusal \
  'published binding is not the staged directory inode'
run_barrier_mutation publication-published-symlink destination-published \
  published-name-symlink refusal 'published binding is not a directory'

echo "component-manifests-test: exact set, mismatch refusals, pinned mutation refusal, ABA safety, and byte-exact publication passed"
