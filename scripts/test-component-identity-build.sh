#!/usr/bin/env bash
# #952 release/adversarial matrix for the generic single-root builder.

set -euo pipefail

ROOT=$(git rev-parse --show-toplevel)
cd "$ROOT"
TMP=$(mktemp -d "${TMPDIR:-/tmp}/nmp-component-v2.XXXXXX")
CORE_ARTIFACT_ONE=
CORE_ARTIFACT_TWO=
PROVIDER_ARTIFACT=
PROVIDER_DYNAMIC_ARTIFACT=
LOCK_HOLDER_PID=
cleanup() {
  if [[ -n "$LOCK_HOLDER_PID" ]]; then
    printf '%s\n' release >"$TMP/lock-release" 2>/dev/null || true
    kill "$LOCK_HOLDER_PID" 2>/dev/null || true
    wait "$LOCK_HOLDER_PID" 2>/dev/null || true
  fi
  local directory
  for directory in \
    "$CORE_ARTIFACT_ONE" \
    "$CORE_ARTIFACT_TWO" \
    "$PROVIDER_ARTIFACT" \
    "$PROVIDER_DYNAMIC_ARTIFACT"; do
    if [[ -n "$directory" && -d "$directory" ]]; then
      chmod -R u+w "$directory" 2>/dev/null || true
      rm -r "$directory"
    fi
  done
  chmod -R u+w "$TMP" 2>/dev/null || true
  rm -r "$TMP"
}
trap cleanup EXIT

TARGET_DIR_VALUE=${CARGO_TARGET_DIR:-target}
if [[ "$TARGET_DIR_VALUE" == /* ]]; then
  BASE_TARGET_DIR=$TARGET_DIR_VALUE
else
  BASE_TARGET_DIR="$ROOT/$TARGET_DIR_VALUE"
fi
HOST_TARGET=$(rustc -vV | sed -n 's/^host: //p')
[[ -n "$HOST_TARGET" ]]
case "$(uname -s)" in
  Darwin) DYNAMIC_EXTENSION=dylib ;;
  Linux) DYNAMIC_EXTENSION=so ;;
  *) echo "component-identity-build: unsupported host" >&2; exit 1 ;;
esac
cargo fetch --locked

CORE_TARGET_DIR="$BASE_TARGET_DIR/nmp-component-build-v2/nmp-core"
mkdir -p "$CORE_TARGET_DIR"
printf '%s\n' stale-bytes-without-a-kernel-lock >"$CORE_TARGET_DIR/.builder-lock"

CORE_ARTIFACT_ONE=$(
  scripts/build-component-release.sh "$BASE_TARGET_DIR" "$HOST_TARGET" nmp-ffi
)
PROVIDER_ARTIFACT=$(
  scripts/build-component-release.sh "$BASE_TARGET_DIR" "$HOST_TARGET" \
    --core-artifact "$CORE_ARTIFACT_ONE/libnmp_ffi.a" nmp-nip46-ffi
)
PROVIDER_DYNAMIC_ARTIFACT=$(
  scripts/build-component-release.sh "$BASE_TARGET_DIR" "$HOST_TARGET" \
    --core-artifact "$CORE_ARTIFACT_ONE/libnmp_ffi.$DYNAMIC_EXTENSION" \
    nmp-nip46-ffi
)
CORE_ARTIFACT_TWO=$(
  scripts/build-component-release.sh "$BASE_TARGET_DIR" "$HOST_TARGET" nmp-ffi
)

cmp "$CORE_ARTIFACT_ONE/component-manifest.json" \
  "$CORE_ARTIFACT_TWO/component-manifest.json"
scripts/verify-component-manifests.py \
  "$CORE_ARTIFACT_ONE/component-manifest.json" \
  "$PROVIDER_ARTIFACT/component-manifest.json" >/dev/null

WITNESS_BIN="$BASE_TARGET_DIR/nmp-component-artifact-witness-tool/release/nmp-component-artifact-witness"
LOCALIZATION_SYMBOLS="$TMP/interface-symbols.nul"
LOCALIZATION_PLAN="$TMP/interface-plan.json"
"$WITNESS_BIN" plan-localization \
  --artifact "$CORE_ARTIFACT_ONE/libnmp_ffi.a" \
  --target "$HOST_TARGET" \
  --interface-namespace nmp_component_interface \
  --out "$LOCALIZATION_SYMBOLS" >"$LOCALIZATION_PLAN"
cmp "$LOCALIZATION_PLAN" \
  "$PROVIDER_ARTIFACT/component-interface-localization-plan.json"
cmp "$LOCALIZATION_PLAN" \
  "$PROVIDER_DYNAMIC_ARTIFACT/component-interface-localization-plan.json"
chmod a-w "$LOCALIZATION_SYMBOLS" "$LOCALIZATION_PLAN"

artifact_verify_refuses() {
  local name=$1 expected=$2
  shift 2
  if scripts/verify-component-manifests.py "$@" >"$TMP/$name.out" 2>&1; then
    echo "component-identity-build: artifact mutation $name passed" >&2
    exit 1
  fi
  grep -qF "$expected" "$TMP/$name.out"
}

scripts/verify-component-manifests.py \
  --witness-tool "$WITNESS_BIN" \
  --artifact "$CORE_ARTIFACT_ONE/libnmp_ffi.a" \
  "$CORE_ARTIFACT_ONE/component-manifest.json" \
  --artifact "$PROVIDER_ARTIFACT/libnmp_nip46_ffi.a" \
  "$PROVIDER_ARTIFACT/component-manifest.json" \
  --forbid-symbols "$LOCALIZATION_SYMBOLS" \
  --localization-source "$CORE_ARTIFACT_ONE/libnmp_ffi.a" \
  --localization-plan "$LOCALIZATION_PLAN" >/dev/null

mkdir "$TMP/byte-flip"
cp "$CORE_ARTIFACT_ONE/libnmp_ffi.a" \
  "$CORE_ARTIFACT_ONE/libnmp_ffi.a.witness.json" \
  "$CORE_ARTIFACT_ONE/component-manifest.json" \
  "$TMP/byte-flip/"
chmod u+w "$TMP/byte-flip/libnmp_ffi.a"
python3 - "$TMP/byte-flip/libnmp_ffi.a" <<'PY'
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
data = bytearray(path.read_bytes())
if data[:8] != b"!<arch>\n":
    raise SystemExit("expected an ordinary static archive")
data[24] = ord("1") if data[24] != ord("1") else ord("2")
path.write_bytes(data)
PY
chmod -R a-w "$TMP/byte-flip"
artifact_verify_refuses byte-flip 'stored witness disagrees' \
  --witness-tool "$WITNESS_BIN" \
  --artifact "$TMP/byte-flip/libnmp_ffi.a" \
  "$TMP/byte-flip/component-manifest.json"

artifact_verify_refuses manifest-artifact-swap \
  'artifact name does not match manifest library_stem' \
  --witness-tool "$WITNESS_BIN" \
  --artifact "$CORE_ARTIFACT_ONE/libnmp_ffi.a" \
  "$CORE_ARTIFACT_ONE/component-manifest.json" \
  --artifact "$PROVIDER_ARTIFACT/libnmp_nip46_ffi.a" \
  "$CORE_ARTIFACT_ONE/component-manifest.json"

cp "$PROVIDER_ARTIFACT/component-manifest.json" "$TMP/provider-lie.json"
chmod u+w "$TMP/provider-lie.json"
python3 - "$TMP/provider-lie.json" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
value = json.loads(path.read_text())
identity = "nmp-nip46-component-v2-" + "a" * 64
value["identity"] = identity
value["binding_identity"] = identity
value["native_identity"] = identity
path.write_text(json.dumps(value, separators=(",", ":"), sort_keys=True) + "\n")
PY
chmod a-w "$TMP/provider-lie.json"
artifact_verify_refuses coordinated-json-lie \
  'attestation identity disagrees with manifest' \
  --witness-tool "$WITNESS_BIN" \
  --artifact "$CORE_ARTIFACT_ONE/libnmp_ffi.a" \
  "$CORE_ARTIFACT_ONE/component-manifest.json" \
  --artifact "$PROVIDER_ARTIFACT/libnmp_nip46_ffi.a" \
  "$TMP/provider-lie.json" \
  --forbid-symbols "$LOCALIZATION_SYMBOLS" \
  --localization-source "$CORE_ARTIFACT_ONE/libnmp_ffi.a" \
  --localization-plan "$LOCALIZATION_PLAN"

for field in uniffi_namespace graph_digest; do
  mutation="$TMP/provider-$field-lie.json"
  cp "$PROVIDER_ARTIFACT/component-manifest.json" "$mutation"
  chmod u+w "$mutation"
  python3 - "$mutation" "$field" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
field = sys.argv[2]
value = json.loads(path.read_text())
value[field] = "wrong_namespace" if field == "uniffi_namespace" else "0" * 64
path.write_text(json.dumps(value, separators=(",", ":"), sort_keys=True) + "\n")
PY
  chmod a-w "$mutation"
  expected="attestation $field disagrees with manifest"
  if [[ "$field" == uniffi_namespace ]]; then
    expected='expected one compiled component for manifest namespace'
  fi
  artifact_verify_refuses "provider-$field-lie" \
    "$expected" \
    --witness-tool "$WITNESS_BIN" \
    --artifact "$CORE_ARTIFACT_ONE/libnmp_ffi.a" \
    "$CORE_ARTIFACT_ONE/component-manifest.json" \
    --artifact "$PROVIDER_ARTIFACT/libnmp_nip46_ffi.a" "$mutation" \
    --forbid-symbols "$LOCALIZATION_SYMBOLS" \
    --localization-source "$CORE_ARTIFACT_ONE/libnmp_ffi.a" \
    --localization-plan "$LOCALIZATION_PLAN"
done

mkdir "$TMP/direct-cross-format"
cp "$CORE_ARTIFACT_ONE/libnmp_ffi.a" \
  "$TMP/direct-cross-format/libnmp_ffi.so"
chmod -R a-w "$TMP/direct-cross-format"
artifact_verify_refuses direct-cross-format \
  'structural format' \
  --witness-tool "$WITNESS_BIN" \
  --artifact "$TMP/direct-cross-format/libnmp_ffi.so" \
  "$CORE_ARTIFACT_ONE/component-manifest.json" \
  --witness "$CORE_ARTIFACT_ONE/libnmp_ffi.a.witness.json"

mkdir "$TMP/core-b"
cp "$CORE_ARTIFACT_ONE/libnmp_ffi.a" \
  "$CORE_ARTIFACT_ONE/component-manifest.json" \
  "$TMP/core-b/"
chmod u+w "$TMP/core-b/libnmp_ffi.a"
python3 - "$TMP/core-b/libnmp_ffi.a" <<'PY'
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
data = bytearray(path.read_bytes())
data[24] = ord("3") if data[24] != ord("3") else ord("4")
path.write_bytes(data)
PY
"$WITNESS_BIN" witness \
  --artifact "$TMP/core-b/libnmp_ffi.a" \
  --target "$HOST_TARGET" \
  --component-key nmp-core \
  --attestation-symbol NMP_CORE_COMPONENT_ATTESTATION_V2 \
  >"$TMP/core-b/libnmp_ffi.a.witness.json"
"$WITNESS_BIN" plan-localization \
  --artifact "$TMP/core-b/libnmp_ffi.a" \
  --target "$HOST_TARGET" \
  --interface-namespace nmp_component_interface \
  --out "$TMP/core-b-symbols.nul" >"$TMP/core-b-plan.json"
chmod -R a-w "$TMP/core-b"
chmod a-w "$TMP/core-b-symbols.nul" "$TMP/core-b-plan.json"
artifact_verify_refuses provider-a-core-b \
  'selects 0 supplied core artifacts' \
  --witness-tool "$WITNESS_BIN" \
  --artifact "$TMP/core-b/libnmp_ffi.a" \
  "$TMP/core-b/component-manifest.json" \
  --artifact "$PROVIDER_ARTIFACT/libnmp_nip46_ffi.a" \
  "$PROVIDER_ARTIFACT/component-manifest.json" \
  --forbid-symbols "$TMP/core-b-symbols.nul" \
  --localization-source "$TMP/core-b/libnmp_ffi.a" \
  --localization-plan "$TMP/core-b-plan.json"

artifact_verify_refuses cross-format-swap \
  'selects 0 supplied core artifacts' \
  --witness-tool "$WITNESS_BIN" \
  --artifact "$CORE_ARTIFACT_ONE/libnmp_ffi.a" \
  "$CORE_ARTIFACT_ONE/component-manifest.json" \
  --artifact \
  "$PROVIDER_DYNAMIC_ARTIFACT/libnmp_nip46_ffi.$DYNAMIC_EXTENSION" \
  "$PROVIDER_DYNAMIC_ARTIFACT/component-manifest.json" \
  --forbid-symbols "$LOCALIZATION_SYMBOLS" \
  --localization-source "$CORE_ARTIFACT_ONE/libnmp_ffi.a" \
  --localization-plan "$LOCALIZATION_PLAN"

mkdir "$TMP/coordinated-plan-lie"
RAW_PROVIDER="$BASE_TARGET_DIR/nmp-component-build-v2/nmp-nip46/$HOST_TARGET/release/libnmp_nip46_ffi.a"
cp "$RAW_PROVIDER" "$TMP/coordinated-plan-lie/libnmp_nip46_ffi.a"
"$WITNESS_BIN" witness \
  --artifact "$TMP/coordinated-plan-lie/libnmp_nip46_ffi.a" \
  --target "$HOST_TARGET" \
  --component-key nmp-nip46 \
  --attestation-symbol NMP_NIP46_COMPONENT_ATTESTATION_V2 \
  >"$TMP/coordinated-plan-lie/libnmp_nip46_ffi.a.witness.json"
python3 - \
  "$LOCALIZATION_PLAN" \
  "$LOCALIZATION_SYMBOLS" \
  "$TMP/coordinated-plan-lie/plan.json" \
  "$TMP/coordinated-plan-lie/symbols.nul" <<'PY'
import json
import pathlib
import sys

plan = json.loads(pathlib.Path(sys.argv[1]).read_text())
symbols = [item for item in pathlib.Path(sys.argv[2]).read_bytes().split(b"\0") if item]
plan["symbols"] = plan["symbols"][1:]
pathlib.Path(sys.argv[3]).write_text(
    json.dumps(plan, separators=(",", ":"), sort_keys=True) + "\n"
)
pathlib.Path(sys.argv[4]).write_bytes(b"\0".join(symbols[1:]) + b"\0")
PY
chmod -R a-w "$TMP/coordinated-plan-lie"
artifact_verify_refuses raw-provider-without-localization \
  'optional artifacts require exact localization provenance' \
  --witness-tool "$WITNESS_BIN" \
  --artifact "$CORE_ARTIFACT_ONE/libnmp_ffi.a" \
  "$CORE_ARTIFACT_ONE/component-manifest.json" \
  --artifact "$TMP/coordinated-plan-lie/libnmp_nip46_ffi.a" \
  "$PROVIDER_ARTIFACT/component-manifest.json"
artifact_verify_refuses coordinated-binary-witness-plan-lie \
  'saved localization plan disagrees with the witnessed core source' \
  --witness-tool "$WITNESS_BIN" \
  --artifact "$CORE_ARTIFACT_ONE/libnmp_ffi.a" \
  "$CORE_ARTIFACT_ONE/component-manifest.json" \
  --artifact "$TMP/coordinated-plan-lie/libnmp_nip46_ffi.a" \
  "$PROVIDER_ARTIFACT/component-manifest.json" \
  --forbid-symbols "$TMP/coordinated-plan-lie/symbols.nul" \
  --localization-source "$CORE_ARTIFACT_ONE/libnmp_ffi.a" \
  --localization-plan "$TMP/coordinated-plan-lie/plan.json"

if scripts/build-component-release.sh "$BASE_TARGET_DIR" "$HOST_TARGET" \
  nmp-nip46-ffi >"$TMP/no-artifact" 2>&1; then
  echo "component-identity-build: optional build without core artifact passed" >&2
  exit 1
fi
grep -qF 'requires --core-artifact' "$TMP/no-artifact"

mkdir "$TMP/tampered-core"
cp "$CORE_ARTIFACT_ONE/libnmp_ffi.a" \
  "$CORE_ARTIFACT_ONE/libnmp_ffi.a.witness.json" \
  "$CORE_ARTIFACT_ONE/component-manifest.json" \
  "$TMP/tampered-core/"
chmod u+w "$TMP/tampered-core/component-manifest.json"
python3 - "$TMP/tampered-core/component-manifest.json" <<'PY'
import json, pathlib, sys
value = json.loads(pathlib.Path(sys.argv[1]).read_text())
value["interface_identity"] = "nmp-component-interface-v2-" + "0" * 64
pathlib.Path(sys.argv[1]).write_text(
    json.dumps(value, separators=(",", ":"), sort_keys=True) + "\n"
)
PY
chmod -R a-w "$TMP/tampered-core"
if scripts/build-component-release.sh "$BASE_TARGET_DIR" "$HOST_TARGET" \
  --core-artifact "$TMP/tampered-core/libnmp_ffi.a" nmp-nip46-ffi \
  >"$TMP/tampered-output" 2>&1; then
  echo "component-identity-build: tampered core manifest passed" >&2
  exit 1
fi
grep -qF 'nmp-component-interface-v2-00000000' "$TMP/tampered-output"

if env -u NMP_COMPONENT_BUILD_AUTH -u NMP_COMPONENT_BUILD_ROOT \
  CARGO_TARGET_DIR="$CORE_TARGET_DIR" \
  cargo build --locked -p nmp-ffi --release --target "$HOST_TARGET" \
  >"$TMP/unmanaged-output" 2>&1; then
  echo "component-identity-build: unmanaged release passed" >&2
  exit 1
fi
grep -qF 'release native components require the managed builder' "$TMP/unmanaged-output"

mkfifo "$TMP/lock-ready" "$TMP/lock-release"
perl -MFcntl=:flock -e '
  my ($lock_file, $ready_pipe, $release_pipe) = @ARGV;
  open(my $lock, ">>", $lock_file) or die "open $lock_file: $!\n";
  flock($lock, LOCK_EX) or die "lock $lock_file: $!\n";
  open(my $ready, ">", $ready_pipe) or die "open $ready_pipe: $!\n";
  print {$ready} "ready\n";
  close($ready);
  open(my $release, "<", $release_pipe) or die "open $release_pipe: $!\n";
  <$release>;
' "$CORE_TARGET_DIR/.builder-lock" "$TMP/lock-ready" "$TMP/lock-release" &
LOCK_HOLDER_PID=$!
IFS= read -r ready <"$TMP/lock-ready"
[[ "$ready" == ready ]]
if scripts/build-component-release.sh "$BASE_TARGET_DIR" "$HOST_TARGET" nmp-ffi \
  >"$TMP/concurrent-output" 2>&1; then
  echo "component-identity-build: concurrent builder passed" >&2
  exit 1
fi
printf '%s\n' release >"$TMP/lock-release"
wait "$LOCK_HOLDER_PID" 2>/dev/null || true
LOCK_HOLDER_PID=
grep -qF 'another supported nmp-core build is already using' "$TMP/concurrent-output"

for root in nmp-core nmp-nip46; do
  [[ ! -e "$BASE_TARGET_DIR/nmp-component-build-v2/$root/.nmp-component-build-v2/.authorization" ]]
done

echo "component-identity-build: stable core, exact provider requirement, tamper/unmanaged/lock refusals, and sealed manifests passed"
