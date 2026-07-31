#!/usr/bin/env bash
# Build one declared native component root and seal its exact manifest.

set -euo pipefail

usage() {
  echo "usage: $0 BASE_TARGET_DIR TARGET [--core-artifact CORE_LIBRARY] CARGO_PACKAGE" >&2
  exit 2
}

[[ $# -ge 3 ]] || usage
ORIGINAL_ARGS=("$@")
BASE_TARGET_DIR=$1
TARGET=$2
shift 2
CORE_ARTIFACT=
CORE_MANIFEST=
if [[ ${1:-} == --core-artifact ]]; then
  [[ $# -ge 3 ]] || usage
  CORE_ARTIFACT=$2
  shift 2
fi
[[ $# -eq 1 ]] || usage
CARGO_PACKAGE=$1

command -v python3 >/dev/null 2>&1 || {
  echo "component-build: python3 is required" >&2
  exit 1
}

METADATA_ROW=$(
  cargo metadata --locked --format-version 1 --no-deps |
    python3 -c '
import json, re, sys
name = sys.argv[1]
matches = [package for package in json.load(sys.stdin)["packages"] if package["name"] == name]
if len(matches) != 1:
    raise SystemExit(f"component-build: expected one package {name}, found {len(matches)}")
metadata = matches[0].get("metadata", {}).get("nmp-component")
if not isinstance(metadata, dict):
    raise SystemExit(f"component-build: {name} has no [package.metadata.nmp-component]")
required = ("schema", "key", "kind", "library-stem", "uniffi-namespace", "bindgen-bin")
missing = [field for field in required if field not in metadata]
if missing:
    raise SystemExit(f"component-build: {name} metadata lacks {missing}")
if metadata["schema"] != 1 or metadata["kind"] not in ("core", "optional"):
    raise SystemExit(f"component-build: {name} has unsupported component schema/kind")
if not re.fullmatch(r"[a-z][a-z0-9]*(?:-[a-z0-9]+)*", metadata["key"]):
    raise SystemExit(f"component-build: {name} has unstable component key")
for field in ("library-stem", "uniffi-namespace", "bindgen-bin"):
    if not isinstance(metadata[field], str) or not re.fullmatch(r"[A-Za-z0-9_.-]+", metadata[field]):
        raise SystemExit(f"component-build: {name} has invalid {field}")
print("\t".join((
    metadata["key"], metadata["kind"], metadata["library-stem"],
    metadata["uniffi-namespace"], metadata["bindgen-bin"],
    metadata.get("metadata-audit-bin", ""),
)))
' "$CARGO_PACKAGE"
)
IFS=$'\t' read -r COMPONENT_KEY COMPONENT_KIND LIBRARY_STEM \
  UNIFFI_NAMESPACE BINDGEN_BIN METADATA_AUDIT_BIN <<EOF
$METADATA_ROW
EOF

if [[ $COMPONENT_KIND == core && -n $CORE_ARTIFACT ]]; then
  echo "component-build: core must not receive --core-artifact" >&2
  exit 1
fi
if [[ $COMPONENT_KIND == optional && -z $CORE_ARTIFACT ]]; then
  echo "component-build: optional $COMPONENT_KEY requires --core-artifact" >&2
  exit 1
fi
if [[ -n $CORE_ARTIFACT ]]; then
  CORE_ARTIFACT=$(cd "$(dirname "$CORE_ARTIFACT")" && pwd)/$(basename "$CORE_ARTIFACT")
  [[ -f $CORE_ARTIFACT ]] || {
    echo "component-build: core artifact is not a regular file: $CORE_ARTIFACT" >&2
    exit 1
  }
  CORE_MANIFEST=$(dirname "$CORE_ARTIFACT")/component-manifest.json
  CORE_WITNESS="$CORE_ARTIFACT.witness.json"
  [[ -f $CORE_MANIFEST ]] || {
    echo "component-build: core artifact has no adjacent manifest: $CORE_MANIFEST" >&2
    exit 1
  }
  [[ -f $CORE_WITNESS ]] || {
    echo "component-build: core artifact has no adjacent witness: $CORE_WITNESS" >&2
    exit 1
  }
fi

COMPONENT_TARGET_DIR="$BASE_TARGET_DIR/nmp-component-build-v2/$COMPONENT_KEY"
MARKER_DIR="$COMPONENT_TARGET_DIR/.nmp-component-build-v2"
MARKER="$MARKER_DIR/$TARGET"
AUTHORIZATION="$MARKER_DIR/.authorization"
MANIFEST_OUTPUT="$MARKER_DIR/$TARGET.manifest.json"
LOCK_FILE="$COMPONENT_TARGET_DIR/.builder-lock"
ARTIFACT_PARENT="$BASE_TARGET_DIR/nmp-component-artifacts-v2/$COMPONENT_KEY"
ARTIFACT_SNAPSHOT=
mkdir -p "$MARKER_DIR" "$ARTIFACT_PARENT"

if [[ ${NMP_COMPONENT_BUILD_LOCK_HELD:-} != 1 ]]; then
  command -v perl >/dev/null 2>&1 || {
    echo "component-build: perl is required for the component lock" >&2
    exit 1
  }
  exec perl -MFcntl=:flock,F_SETFD -e '
    my ($lock_file, $component, $target_dir, @command) = @ARGV;
    open(my $lock, ">>", $lock_file)
      or die "component-build: open $lock_file: $!\n";
    flock($lock, LOCK_EX | LOCK_NB)
      or die "component-build: another supported $component build is already using $target_dir\n";
    fcntl($lock, F_SETFD, 0)
      or die "component-build: preserve component lock across exec: $!\n";
    $ENV{NMP_COMPONENT_BUILD_LOCK_HELD} = "1";
    exec @command
      or die "component-build: restart under component lock: $!\n";
  ' "$LOCK_FILE" "$COMPONENT_KEY" "$COMPONENT_TARGET_DIR" "$0" "${ORIGINAL_ARGS[@]}"
fi

cleanup() {
  local exit_code=$?
  rm -f "$AUTHORIZATION"
  if [[ $exit_code -ne 0 && -n "$ARTIFACT_SNAPSHOT" && -d "$ARTIFACT_SNAPSHOT" ]]; then
    chmod -R u+w "$ARTIFACT_SNAPSHOT" 2>/dev/null || true
    rm -r "$ARTIFACT_SNAPSHOT"
  fi
  exit "$exit_code"
}
trap cleanup EXIT

TEMP_MARKER="$MARKER.tmp.$$"
printf '%s\n' \
  "nmp-component-build-v2" \
  "component-key=$COMPONENT_KEY" \
  "cargo-package=$CARGO_PACKAGE" \
  "target=$TARGET" \
  "profile=release" >"$TEMP_MARKER"
mv "$TEMP_MARKER" "$MARKER"

AUTH_TEMP=$(mktemp "$MARKER_DIR/.authorization.XXXXXX")
AUTH_TOKEN=$(basename "$AUTH_TEMP")
printf '%s\n' "$AUTH_TOKEN" >"$AUTH_TEMP"
mv "$AUTH_TEMP" "$AUTHORIZATION"
rm -f "$MANIFEST_OUTPUT"

WITNESS_MANIFEST="$(
  cd "$(dirname "$0")/../tools/component-artifact-witness"
  pwd
)/Cargo.toml"
WITNESS_TARGET_DIR="$BASE_TARGET_DIR/nmp-component-artifact-witness-tool"
cargo build --manifest-path "$WITNESS_MANIFEST" --locked --release \
  --target-dir "$WITNESS_TARGET_DIR" 1>&2
WITNESS_BIN="$WITNESS_TARGET_DIR/release/nmp-component-artifact-witness"
[[ -x $WITNESS_BIN ]] || {
  echo "component-build: artifact witness tool was not built" >&2
  exit 1
}

LOCALIZATION_SOURCE=
LOCALIZATION_SYMBOLS=
LOCALIZATION_PLAN=
if [[ $COMPONENT_KIND == optional ]]; then
  LOCALIZATION_SOURCE="$(dirname "$CORE_ARTIFACT")/libnmp_ffi.a"
  [[ -f $LOCALIZATION_SOURCE ]] || {
    echo "component-build: sealed core has no static localization authority" >&2
    exit 1
  }
  LOCALIZATION_SYMBOLS="$MARKER_DIR/$TARGET.interface-symbols.nul"
  LOCALIZATION_PLAN="$MARKER_DIR/$TARGET.localization-plan.json"
  LOCALIZATION_PLAN_TEMP="$LOCALIZATION_PLAN.tmp.$$"
  "$WITNESS_BIN" plan-localization \
    --artifact "$LOCALIZATION_SOURCE" \
    --target "$TARGET" \
    --interface-namespace nmp_component_interface \
    --out "$LOCALIZATION_SYMBOLS" >"$LOCALIZATION_PLAN_TEMP"
  mv "$LOCALIZATION_PLAN_TEMP" "$LOCALIZATION_PLAN"
  chmod a-w "$LOCALIZATION_SYMBOLS" "$LOCALIZATION_PLAN"
fi

BUILD_ENV=(
  "CARGO_TARGET_DIR=$COMPONENT_TARGET_DIR"
  "NMP_COMPONENT_BUILD_ROOT=$COMPONENT_TARGET_DIR"
  "NMP_COMPONENT_BUILD_AUTH=$AUTH_TOKEN"
  "NMP_COMPONENT_MANIFEST_OUTPUT=$MANIFEST_OUTPUT"
)
if [[ -n $CORE_ARTIFACT ]]; then
  BUILD_ENV+=("NMP_COMPONENT_CORE_ARTIFACT=$CORE_ARTIFACT")
fi
env "${BUILD_ENV[@]}" \
  cargo build --frozen -p "$CARGO_PACKAGE" --release --target "$TARGET" 1>&2

RELEASE_DIR="$COMPONENT_TARGET_DIR/$TARGET/release"
LIBRARIES=()
for extension in a so dylib; do
  [[ -f "$RELEASE_DIR/lib$LIBRARY_STEM.$extension" ]] &&
    LIBRARIES+=("$RELEASE_DIR/lib$LIBRARY_STEM.$extension")
done
[[ ${#LIBRARIES[@]} -gt 0 ]] || {
  echo "component-build: expected lib$LIBRARY_STEM under $RELEASE_DIR" >&2
  exit 1
}
if [[ $COMPONENT_KIND == optional ]]; then
  CORE_EXTENSION=${CORE_ARTIFACT##*.}
  case "$CORE_EXTENSION" in
    a | so | dylib) ;;
    *)
      echo "component-build: unsupported core artifact format: $CORE_ARTIFACT" >&2
      exit 1
      ;;
  esac
  MATCHING_LIBRARY="$RELEASE_DIR/lib$LIBRARY_STEM.$CORE_EXTENSION"
  [[ -f $MATCHING_LIBRARY ]] || {
    echo "component-build: provider has no $CORE_EXTENSION artifact matching its core" >&2
    exit 1
  }
  # One provider attestation binds one exact core digest. Publish only the
  # matching format so a static requirement cannot be mistaken for a dynamic
  # pairing (or vice versa).
  LIBRARIES=("$MATCHING_LIBRARY")
fi
[[ -s $MANIFEST_OUTPUT ]] || {
  echo "component-build: build produced no canonical component manifest" >&2
  exit 1
}

METADATA_AUDIT_EXECUTABLE=
AUTHORITATIVE_CALLABLES=
AUTHORITATIVE_CALLABLE_PLAN=
if [[ -n $METADATA_AUDIT_BIN ]]; then
  echo "component-build: audit compiled $COMPONENT_KEY metadata" >&2
  METADATA_AUDIT_TARGET_DIR="$BASE_TARGET_DIR/nmp-component-metadata-audit-tool/$COMPONENT_KEY"
  cargo build --frozen --quiet -p "$CARGO_PACKAGE" \
    --bin "$METADATA_AUDIT_BIN" \
    --target-dir "$METADATA_AUDIT_TARGET_DIR"
  METADATA_AUDIT_EXECUTABLE="$METADATA_AUDIT_TARGET_DIR/debug/$METADATA_AUDIT_BIN"
  [[ -x $METADATA_AUDIT_EXECUTABLE ]] || {
    echo "component-build: metadata audit executable was not built" >&2
    exit 1
  }
  METADATA_AUDIT_LIBRARY="${LIBRARIES[0]}"
  if [[ $COMPONENT_KIND == optional &&
    -f "$RELEASE_DIR/lib$LIBRARY_STEM.a" ]]; then
    # ELF hides dependency-owned interface symbols at cdylib link time. Audit
    # the companion static output from the same Cargo invocation so the full
    # compiled provider/interface type graph remains available; the final
    # dynamic artifact witness separately requires every provider callable to
    # remain public and every interface-owned symbol to remain hidden.
    METADATA_AUDIT_LIBRARY="$RELEASE_DIR/lib$LIBRARY_STEM.a"
  fi
  scripts/verify-component-manifests.py \
    --metadata-audit-tool "$METADATA_AUDIT_EXECUTABLE" \
    --artifact "$METADATA_AUDIT_LIBRARY" >/dev/null
fi
if [[ $COMPONENT_KIND == optional ]]; then
  [[ -n $METADATA_AUDIT_EXECUTABLE &&
    $METADATA_AUDIT_LIBRARY == "$RELEASE_DIR/lib$LIBRARY_STEM.a" ]] || {
    echo "component-build: optional callable authority requires the audited companion static archive" >&2
    exit 1
  }
  AUTHORITATIVE_CALLABLES="$MARKER_DIR/$TARGET.authoritative-callables.nul"
  AUTHORITATIVE_CALLABLE_PLAN="$MARKER_DIR/$TARGET.authoritative-callables.json"
  AUTHORITATIVE_CALLABLE_PLAN_TEMP="$AUTHORITATIVE_CALLABLE_PLAN.tmp.$$"
  "$WITNESS_BIN" plan-authoritative-callables \
    --artifact "$METADATA_AUDIT_LIBRARY" \
    --target "$TARGET" \
    --component-key "$COMPONENT_KEY" \
    --out "$AUTHORITATIVE_CALLABLES" >"$AUTHORITATIVE_CALLABLE_PLAN_TEMP"
  mv "$AUTHORITATIVE_CALLABLE_PLAN_TEMP" "$AUTHORITATIVE_CALLABLE_PLAN"
  chmod a-w "$AUTHORITATIVE_CALLABLES" "$AUTHORITATIVE_CALLABLE_PLAN"
fi

ARTIFACT_SNAPSHOT=$(mktemp -d "$ARTIFACT_PARENT/$TARGET.XXXXXX")
cp -p "${LIBRARIES[@]}" "$ARTIFACT_SNAPSHOT/"
cp -p "$MANIFEST_OUTPUT" "$ARTIFACT_SNAPSHOT/component-manifest.json"
if [[ $COMPONENT_KIND == optional ]]; then
  # A static app links core and provider archives into one image. Both roots
  # carry their own implementation of the verified shared Rust contract, but
  # core alone owns its public UniFFI C namespace. The structural planner
  # derives the exact raw symbol set from the compiled interface-owned
  # members; transforms consume only that NUL-delimited plan, and the final
  # witness proves the names are no longer public.
  cp -p "$LOCALIZATION_PLAN" \
    "$ARTIFACT_SNAPSHOT/component-interface-localization-plan.json"
  cp -p "$LOCALIZATION_SYMBOLS" \
    "$ARTIFACT_SNAPSHOT/component-interface-forbidden-symbols.nul"

  RUST_SYSROOT=$(rustc --print sysroot)
  RUST_HOST=$(rustc -vV | sed -n 's/^host: //p')
  OBJCOPY="$RUST_SYSROOT/lib/rustlib/$RUST_HOST/bin/rust-objcopy"
  [[ -x $OBJCOPY ]] || {
    echo "component-build: pinned rust-objcopy is required for optional static archives" >&2
    exit 1
  }

  LOCALIZE_ARGUMENTS=()
  while IFS= read -r -d '' symbol; do
    LOCALIZE_ARGUMENTS+=("--localize-symbol=$symbol")
  done <"$LOCALIZATION_SYMBOLS"
  [[ ${#LOCALIZE_ARGUMENTS[@]} -gt 0 ]] || {
    echo "component-build: structural localization plan was empty" >&2
    exit 1
  }

  localize_library() {
    local library=$1
    if [[ $TARGET == *-apple-* && $library == *.dylib ]]; then
      # Mach-O dylibs carry an authoritative export trie. Drive Apple's
      # post-link export transform from the structural raw witness, keeping
      # every public provider symbol except the exact core-owned interface
      # set. The final witness below proves both the provider attestation and
      # the forbidden-symbol absence on the transformed bytes.
      local raw_witness allowlist strip_tool
      raw_witness=$(mktemp "$ARTIFACT_SNAPSHOT/.raw-dylib-witness.XXXXXX")
      allowlist=$(mktemp "$ARTIFACT_SNAPSHOT/.dylib-allowlist.XXXXXX")
      "$WITNESS_BIN" witness \
        --artifact "$library" \
        --target "$TARGET" \
        --component-key "$COMPONENT_KEY" \
        --attestation-symbol NMP_NIP46_COMPONENT_ATTESTATION_V2 \
        >"$raw_witness"
      python3 - "$raw_witness" "$LOCALIZATION_SYMBOLS" "$allowlist" <<'PY'
import json
import pathlib
import sys

witness = json.loads(pathlib.Path(sys.argv[1]).read_text())
public = witness.get("public_symbols")
if not isinstance(public, list) or not all(isinstance(symbol, str) for symbol in public):
    raise SystemExit("component-build: raw dylib witness has no exact public symbol list")
forbidden = {
    symbol.decode("utf-8")
    for symbol in pathlib.Path(sys.argv[2]).read_bytes().split(b"\0")
    if symbol
}
missing = sorted(forbidden - set(public))
if missing:
    raise SystemExit(
        f"component-build: raw provider dylib lacks planned interface symbols: {missing}"
    )
allowed = sorted(set(public) - forbidden)
if not allowed or any("\n" in symbol or "\0" in symbol for symbol in allowed):
    raise SystemExit("component-build: raw provider dylib produced an invalid allowlist")
pathlib.Path(sys.argv[3]).write_text("\n".join(allowed) + "\n")
PY
      strip_tool=$(xcrun --find strip)
      "$strip_tool" -u -r -s "$allowlist" "$library"
      rm -f "$raw_witness" "$allowlist"
      return
    fi

    if [[ $(uname -s) == Darwin ]]; then
      DYLD_LIBRARY_PATH="$RUST_SYSROOT/lib${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}" \
        "$OBJCOPY" "${LOCALIZE_ARGUMENTS[@]}" "$library"
    else
      LD_LIBRARY_PATH="$RUST_SYSROOT/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
        "$OBJCOPY" "${LOCALIZE_ARGUMENTS[@]}" "$library"
    fi

    if [[ $TARGET == *-apple-* ]]; then
      # llvm-objcopy does not reliably localize Mach-O dynamic exports and
      # leaves the section-backed namespace marker external in archives.
      # Clear N_EXT for every exact symbol already named by the structural
      # plan. This script does no discovery; the fresh final witness decides
      # whether the complete forbidden set is absent.
      python3 - "$library" "$LOCALIZATION_SYMBOLS" <<'PY'
import pathlib
import struct
import sys

path = pathlib.Path(sys.argv[1])
symbols_path = pathlib.Path(sys.argv[2])
planned_symbols = {
    symbol
    for symbol in symbols_path.read_bytes().split(b"\0")
    if symbol
}
if not planned_symbols:
    raise SystemExit("component-build: structural Mach-O symbol plan is empty")
data = bytearray(path.read_bytes())

def mach_members():
    if data[:4] == b"\xcf\xfa\xed\xfe":
        yield 0
        return
    if data[:8] != b"!<arch>\n":
        raise SystemExit(f"component-build: expected a Mach-O binary or archive: {path}")
    offset = 8
    while offset < len(data):
        header = data[offset:offset + 60]
        if len(header) != 60 or header[58:60] != b"`\n":
            raise SystemExit(f"component-build: malformed archive member header: {path}")
        try:
            size = int(header[48:58].decode("ascii").strip())
        except ValueError as error:
            raise SystemExit(
                f"component-build: malformed archive member size: {path}"
            ) from error
        payload = offset + 60
        end = payload + size
        if end > len(data):
            raise SystemExit(f"component-build: truncated archive member: {path}")
        object_start = payload
        name = header[:16].decode("ascii", errors="strict").strip()
        if name.startswith("#1/"):
            try:
                name_length = int(name[3:])
            except ValueError as error:
                raise SystemExit(
                    f"component-build: malformed BSD archive name: {path}"
                ) from error
            object_start += name_length
        if data[object_start:object_start + 4] == b"\xcf\xfa\xed\xfe":
            yield object_start
        offset = end + (size & 1)

found = {symbol: 0 for symbol in planned_symbols}
for base in mach_members():
    if base + 32 > len(data):
        raise SystemExit(f"component-build: truncated Mach-O header: {path}")
    _, _, _, _, command_count, _, _, _ = struct.unpack_from("<8I", data, base)
    command_offset = base + 32
    table = None
    for _ in range(command_count):
        if command_offset + 8 > len(data):
            raise SystemExit(f"component-build: truncated Mach-O commands: {path}")
        command, size = struct.unpack_from("<2I", data, command_offset)
        if size < 8 or command_offset + size > len(data):
            raise SystemExit(f"component-build: invalid Mach-O command size: {path}")
        if command == 0x2:
            _, _, symbol_table_offset, count, string_table_offset, string_size = struct.unpack_from(
                "<6I", data, command_offset
            )
            table = (
                base + symbol_table_offset,
                count,
                base + string_table_offset,
                string_size,
            )
            break
        command_offset += size
    if table is None:
        continue
    symbol_table, count, string_table, string_size = table
    for index in range(count):
        entry = symbol_table + index * 16
        if entry + 16 > len(data):
            raise SystemExit(f"component-build: truncated Mach-O symbol table: {path}")
        string_index, symbol_type, _, _, _ = struct.unpack_from("<IBBHQ", data, entry)
        if not 0 < string_index < string_size:
            continue
        start = string_table + string_index
        end = data.find(b"\0", start, string_table + string_size)
        name = bytes(data[start:end]) if end >= 0 else b""
        if name in planned_symbols:
            data[entry + 4] = symbol_type & ~0x01
            found[name] += 1
unexpected = {
    name.decode("utf-8", errors="replace"): count
    for name, count in found.items()
    if count != 1
}
if unexpected:
    raise SystemExit(
        "component-build: planned Mach-O symbol occurrence counts disagree: "
        f"{unexpected}"
    )
path.write_bytes(data)
PY
    fi
    if [[ $library == *.a ]]; then
      ranlib "$library"
    fi
  }

  for library in "$ARTIFACT_SNAPSHOT"/lib"$LIBRARY_STEM".{a,so,dylib}; do
    [[ -f $library ]] && localize_library "$library"
  done
fi

ATTESTATION_SYMBOL=NMP_CORE_COMPONENT_ATTESTATION_V2
if [[ $COMPONENT_KIND == optional ]]; then
  ATTESTATION_SYMBOL=NMP_NIP46_COMPONENT_ATTESTATION_V2
fi
for library in "$ARTIFACT_SNAPSHOT"/lib"$LIBRARY_STEM".{a,so,dylib}; do
  [[ -f $library ]] || continue
  witness="$library.witness.json"
  witness_temporary="$witness.tmp"
  if [[ $COMPONENT_KIND == optional ]]; then
    "$WITNESS_BIN" witness \
      --artifact "$library" \
      --target "$TARGET" \
      --component-key "$COMPONENT_KEY" \
      --attestation-symbol "$ATTESTATION_SYMBOL" \
      --forbid-symbols "$LOCALIZATION_SYMBOLS" \
      --require-callables "$AUTHORITATIVE_CALLABLES" >"$witness_temporary"
  else
    "$WITNESS_BIN" witness \
      --artifact "$library" \
      --target "$TARGET" \
      --component-key "$COMPONENT_KEY" \
      --attestation-symbol "$ATTESTATION_SYMBOL" >"$witness_temporary"
  fi
  mv "$witness_temporary" "$witness"
done
if [[ -f "$RELEASE_DIR/$BINDGEN_BIN" ]]; then
  cp -p "$RELEASE_DIR/$BINDGEN_BIN" "$ARTIFACT_SNAPSHOT/"
elif [[ -f "$RELEASE_DIR/$BINDGEN_BIN.exe" ]]; then
  cp -p "$RELEASE_DIR/$BINDGEN_BIN.exe" "$ARTIFACT_SNAPSHOT/"
fi

# Verification consumes the already sealed snapshot. Read-only files prevent
# byte edits and a read-only artifact directory prevents name replacement
# between the fresh structural reads.
chmod -R a-w "$ARTIFACT_SNAPSHOT"

VERIFY_ARGUMENTS=(--witness-tool "$WITNESS_BIN")
if [[ $COMPONENT_KIND == optional ]]; then
  VERIFY_ARGUMENTS+=(--artifact "$CORE_ARTIFACT" "$CORE_MANIFEST")
  if [[ $LOCALIZATION_SOURCE != "$CORE_ARTIFACT" ]]; then
    VERIFY_ARGUMENTS+=(--artifact "$LOCALIZATION_SOURCE" "$CORE_MANIFEST")
  fi
fi
for library in "$ARTIFACT_SNAPSHOT"/lib"$LIBRARY_STEM".{a,so,dylib}; do
  [[ -f $library ]] || continue
  VERIFY_ARGUMENTS+=(--artifact "$library" "$ARTIFACT_SNAPSHOT/component-manifest.json")
  if [[ $COMPONENT_KIND == optional ]]; then
    VERIFY_ARGUMENTS+=(
      --forbid-symbols \
      "$ARTIFACT_SNAPSHOT/component-interface-forbidden-symbols.nul"
      --localization-source "$LOCALIZATION_SOURCE"
      --localization-plan \
      "$ARTIFACT_SNAPSHOT/component-interface-localization-plan.json"
    )
  fi
done
scripts/verify-component-manifests.py "${VERIFY_ARGUMENTS[@]}" >/dev/null

rm -f "$AUTHORIZATION"
printf '%s\n' "$ARTIFACT_SNAPSHOT"
