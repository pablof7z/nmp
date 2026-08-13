#!/usr/bin/env bash
#
# Mechanical Cross-SDK Parity gate (architecture gate 5).
#
# Each active component is checked independently. Rust FFI vocabulary from a
# component's declared `ffi_sources` may only be satisfied by that component's
# declared Swift/Kotlin roots. This prevents a coincidental word in one
# component from masking a missing projection in another. Intentional modeling
# differences are exact (component, concept, platform) exceptions in
# scripts/check-sdk-parity-allowlist.toml. Exceptions that suppress nothing
# are reported explicitly so obsolete escape hatches cannot rot silently.
#
# This remains a deliberately conservative text heuristic: exported Rust
# type/function identifiers and hand-written SDK identifiers are split into
# concept words. It catches an entirely absent concept, not signature drift.
#
# Usage:
#   scripts/check-sdk-parity.sh
#   scripts/check-sdk-parity.sh --quiet
#
# Exit status:
#   0 parity holds outside exact exceptions
#   1 a non-excepted concept is absent
#   2 the catalog, layout, or extraction is unsound
set -euo pipefail

SCRIPT_PATH=${BASH_SOURCE[0]}
SCRIPT_DIR=${SCRIPT_PATH%/*}
[[ $SCRIPT_DIR != "$SCRIPT_PATH" ]] || SCRIPT_DIR=.
# shellcheck source=scripts/lib/require-commands.sh
source "$SCRIPT_DIR/lib/require-commands.sh" || exit 2
require_commands awk cat comm cut find git grep mkdir mktemp mv python3 rm sed sort tr wc || exit 2

QUIET=0
case ${1:-} in
  --quiet) QUIET=1 ;;
  "") ;;
  *) echo "check-sdk-parity: usage: $0 [--quiet]" >&2; exit 2 ;;
esac

ROOT=${SDK_PARITY_ROOT:-$(git rev-parse --show-toplevel 2>/dev/null || true)}
[[ -n "$ROOT" ]] || {
  echo "check-sdk-parity: could not resolve repository root" >&2
  exit 2
}

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t nmp-sdk-parity)
trap 'rm -rf "$tmp"' EXIT

# #1448 deleted the component/snapshot catalog. Cross-SDK parity still has one
# active component, so this retained gate owns its roots directly instead of
# rebuilding deleted surface-governance machinery.
printf '%s\0' \
  $'nmp-core\tmeta\tnmp_ffi' \
  $'nmp-core\tffi\tcrates/nmp-ffi/src' \
  $'nmp-core\tswift\tPackages/NMP/Sources/NMP' \
  $'nmp-core\tswift\tPackages/NMP/Sources/NMPContent' \
  $'nmp-core\tkotlin\tPackages/NMPKotlin/src/main/kotlin/com/nmp/sdk' \
  > "$tmp/parity-rows"

ALLOWLIST="$ROOT/scripts/check-sdk-parity-allowlist.toml"
python3 - "$ALLOWLIST" > "$tmp/allowlist-rows" <<'PY' || exit 2
import pathlib
import re
import sys
import tomllib

path = pathlib.Path(sys.argv[1])
try:
    document = tomllib.loads(path.read_text(encoding="utf-8"))
except (OSError, UnicodeError, tomllib.TOMLDecodeError) as error:
    print(f"check-sdk-parity: cannot read parity allowlist: {error}", file=sys.stderr)
    raise SystemExit(1)

if set(document) - {"schema", "exception"}:
    print("check-sdk-parity: parity allowlist has unknown top-level fields", file=sys.stderr)
    raise SystemExit(1)
if document.get("schema") != 1:
    print("check-sdk-parity: parity allowlist schema must be 1", file=sys.stderr)
    raise SystemExit(1)

previous = None
seen = set()
for exception in document.get("exception", []):
    if set(exception) != {"component", "concept", "platform", "justification"}:
        print("check-sdk-parity: parity exception fields are incomplete or unknown", file=sys.stderr)
        raise SystemExit(1)
    component = exception["component"]
    concept = exception["concept"]
    platform = exception["platform"]
    justification = exception["justification"]
    if component != "nmp-core":
        print(f"check-sdk-parity: exception names unknown component: {component}", file=sys.stderr)
        raise SystemExit(1)
    if not isinstance(concept, str) or not re.fullmatch(r"[a-z0-9]{3,}", concept):
        print(f"check-sdk-parity: malformed exception concept: {concept}", file=sys.stderr)
        raise SystemExit(1)
    if platform not in {"swift", "kotlin"}:
        print(f"check-sdk-parity: malformed exception platform: {platform}", file=sys.stderr)
        raise SystemExit(1)
    if not isinstance(justification, str) or not justification.strip():
        print("check-sdk-parity: exception justification must not be empty", file=sys.stderr)
        raise SystemExit(1)
    key = (component, concept, platform)
    if key in seen:
        print("check-sdk-parity: duplicate component/concept/platform exception", file=sys.stderr)
        raise SystemExit(1)
    if previous is not None and previous >= key:
        print("check-sdk-parity: exceptions must be in canonical order", file=sys.stderr)
        raise SystemExit(1)
    seen.add(key)
    previous = key
    row = "\t".join((*key, justification)).encode()
    sys.stdout.buffer.write(row + b"\0")
PY

STOPWORDS_RE='^(new|get|set|default|project|op|inner|cancel|disconnect|connect|self|id|ids|ref|mut|arc|box|dyn|into|from|with|and|the|is|as|ok|err|some|none|true|false|str|string|u8|u16|u32|u64|i8|i16|i32|i64|f32|f64|bool|vec|option|result|ffi|nmp|fn|pub|impl|struct|enum|trait|type|let|var|func|class|public|private|internal|open|override|companion|data|sealed|typealias|import|package|extension|protocol|static|on|to|of|in|for|by|it|an|com|sdk|kotlin|swift|clone|debug|eq|partialeq|hash|hashable|sendable|codable|copy|not|test|tests|mod)$'

tokenize() {
  sed -E 's/([a-z0-9])([A-Z])/\1_\2/g; s/([A-Z]+)([A-Z][a-z])/\1_\2/g' \
    | tr '[:upper:]' '[:lower:]' \
    | tr -cs 'a-z0-9' '_' \
    | tr '_' '\n' \
    | grep -v '^$' \
    | grep -vE "$STOPWORDS_RE" \
    | awk 'length($0) >= 3' || true
}

component_dir() {
  printf '%s/components/%s' "$tmp" "$1"
}

while IFS= read -r -d '' row; do
  component=${row%%	*}
  rest=${row#*	}
  kind=${rest%%	*}
  value=${rest#*	}
  dir=$(component_dir "$component")
  mkdir -p "$dir"
  case "$kind" in
    meta) printf '%s\n' "$value" > "$dir/namespace" ;;
    ffi|swift|kotlin) printf '%s\n' "$value" >> "$dir/$kind-roots" ;;
    omit-swift|omit-kotlin) printf '%s\n' "$value" > "$dir/$kind" ;;
    *) echo "check-sdk-parity: unknown catalog row kind: $kind" >&2; exit 2 ;;
  esac
done < "$tmp/parity-rows"

while IFS= read -r -d '' row; do
  component=${row%%	*}
  rest=${row#*	}
  concept=${rest%%	*}
  rest=${rest#*	}
  platform=${rest%%	*}
  dir=$(component_dir "$component")
  [[ -d "$dir" ]] || {
    echo "check-sdk-parity: allowlist row names unknown component: $component" >&2
    exit 2
  }
  printf '%s\n' "$concept" >> "$dir/allow-$platform"
done < "$tmp/allowlist-rows"

extract_rust_symbols() {
  local roots_file=$1 output=$2 files=$3
  : > "$files"
  while IFS= read -r root; do
    [[ -d "$ROOT/$root" ]] || {
      echo "check-sdk-parity: declared Rust source root is missing: $root" >&2
      return 2
    }
    find "$ROOT/$root" -type f -name '*.rs' >> "$files"
  done < "$roots_file"
  LC_ALL=C sort -u "$files" -o "$files"
  [[ -s "$files" ]] || {
    echo "check-sdk-parity: declared Rust roots contain no .rs files" >&2
    return 2
  }

  : > "$output"
  while IFS= read -r file; do
    grep -A1 -E '#\[derive\(.*\b(Object|Enum|Record|Error)\b.*\)\]' "$file" 2>/dev/null \
      | grep -E '^(pub )?(struct|enum) ' \
      | sed -E 's/^(pub )?(struct|enum)[ \t]+([A-Za-z0-9_]+).*/\3/' >> "$output" || true

    capturing=0
    while IFS= read -r line || [[ -n "$line" ]]; do
      if [[ "$line" =~ ^#\[uniffi::export ]]; then
        capturing=1
        continue
      fi
      if [[ $capturing -eq 1 && "$line" =~ ^\} ]]; then
        capturing=0
        continue
      fi
      if [[ $capturing -eq 1 && "$line" =~ (^|[^A-Za-z_])(pub[[:space:]]+)?fn[[:space:]]+([A-Za-z_][A-Za-z0-9_]*) ]]; then
        printf '%s\n' "${BASH_REMATCH[3]}" >> "$output"
      fi
    done < "$file"
  done < "$files"
  LC_ALL=C sort -u "$output" -o "$output"
  [[ -s "$output" ]] || {
    echo "check-sdk-parity: extracted zero Rust FFI symbols" >&2
    return 2
  }
}

extract_sdk_words() {
  local roots_file=$1 extension=$2 output=$3
  local files=$output.files
  local identifiers=$output.identifiers
  : > "$files"
  while IFS= read -r root; do
    [[ -d "$ROOT/$root" ]] || {
      echo "check-sdk-parity: declared SDK source root is missing: $root" >&2
      return 2
    }
    find "$ROOT/$root" -type f -name "*.$extension" \
      -not -path '*/uniffi/*' \
      -not -path '*/NMPFFI/*' >> "$files"
  done < "$roots_file"
  LC_ALL=C sort -u "$files" -o "$files"
  [[ -s "$files" ]] || {
    echo "check-sdk-parity: declared SDK roots contain no .$extension files" >&2
    return 2
  }
  : > "$identifiers"
  while IFS= read -r file; do
    grep -hEo '[A-Za-z_][A-Za-z0-9_]*' "$file" >> "$identifiers" 2>/dev/null || true
  done < "$files"
  tokenize < "$identifiers" | LC_ALL=C sort -u > "$output"
  [[ -s "$output" ]] || {
    echo "check-sdk-parity: extracted zero SDK identifier words" >&2
    return 2
  }
}

report_words() {
  local component=$1 platform=$2 words=$3 map=$4
  while IFS= read -r word; do
    [[ -n "$word" ]] || continue
    example=$(awk -F '	' -v wanted="$word" '$1 == wanted { print $2; exit }' "$map")
    printf '  - %s / %s: %-20s (Rust FFI example: %s)\n' \
      "$component" "$platform" "$word" "$example"
  done < "$words"
}

total_components=0
total_rust_words=0
total_missing_swift=0
total_missing_kotlin=0
total_allowed_swift=0
total_allowed_kotlin=0
total_stale_swift=0
total_stale_kotlin=0

for dir in "$tmp"/components/*; do
  [[ -d "$dir" ]] || {
    echo "check-sdk-parity: catalog has no active components" >&2
    exit 2
  }
  component=${dir##*/}
  total_components=$((total_components + 1))
  [[ -s "$dir/ffi-roots" && -s "$dir/namespace" ]] || {
    echo "check-sdk-parity: incomplete catalog rows for $component" >&2
    exit 2
  }

  extract_rust_symbols "$dir/ffi-roots" "$dir/rust-symbols" "$dir/rust-files" || exit 2
  : > "$dir/rust-map"
  while IFS= read -r symbol; do
    printf '%s\n' "$symbol" | tokenize | while IFS= read -r word; do
      printf '%s\t%s\n' "$word" "$symbol"
    done >> "$dir/rust-map"
  done < "$dir/rust-symbols"
  LC_ALL=C sort -t '	' -k1,1 -u "$dir/rust-map" -o "$dir/rust-map"
  cut -f1 "$dir/rust-map" | LC_ALL=C sort -u > "$dir/rust-words"
  rust_count=$(wc -l < "$dir/rust-words" | tr -d ' ')
  total_rust_words=$((total_rust_words + rust_count))

  for platform in swift kotlin; do
    : > "$dir/allow-$platform.tmp"
    if [[ -f "$dir/allow-$platform" ]]; then
      LC_ALL=C sort -u "$dir/allow-$platform" > "$dir/allow-$platform.tmp"
    fi
    mv "$dir/allow-$platform.tmp" "$dir/allow-$platform"

    if [[ -f "$dir/omit-$platform" ]]; then
      : > "$dir/missing-$platform"
      : > "$dir/allowed-$platform"
      cp "$dir/allow-$platform" "$dir/stale-$platform"
      continue
    fi
    [[ -s "$dir/$platform-roots" ]] || {
      echo "check-sdk-parity: $component has neither $platform roots nor omission" >&2
      exit 2
    }
    extension=swift
    [[ $platform == kotlin ]] && extension=kt
    extract_sdk_words "$dir/$platform-roots" "$extension" "$dir/$platform-words" || exit 2
    comm -23 "$dir/rust-words" "$dir/$platform-words" > "$dir/missing-$platform.raw"
    comm -23 "$dir/missing-$platform.raw" "$dir/allow-$platform" > "$dir/missing-$platform"
    comm -12 "$dir/missing-$platform.raw" "$dir/allow-$platform" > "$dir/allowed-$platform"
    comm -23 "$dir/allow-$platform" "$dir/missing-$platform.raw" > "$dir/stale-$platform"
  done

  missing_swift=$(wc -l < "$dir/missing-swift" | tr -d ' ')
  missing_kotlin=$(wc -l < "$dir/missing-kotlin" | tr -d ' ')
  allowed_swift=$(wc -l < "$dir/allowed-swift" | tr -d ' ')
  allowed_kotlin=$(wc -l < "$dir/allowed-kotlin" | tr -d ' ')
  stale_swift=$(wc -l < "$dir/stale-swift" | tr -d ' ')
  stale_kotlin=$(wc -l < "$dir/stale-kotlin" | tr -d ' ')
  total_missing_swift=$((total_missing_swift + missing_swift))
  total_missing_kotlin=$((total_missing_kotlin + missing_kotlin))
  total_allowed_swift=$((total_allowed_swift + allowed_swift))
  total_allowed_kotlin=$((total_allowed_kotlin + allowed_kotlin))
  total_stale_swift=$((total_stale_swift + stale_swift))
  total_stale_kotlin=$((total_stale_kotlin + stale_kotlin))

  if [[ $QUIET -eq 0 ]]; then
    if (( missing_swift > 0 )); then
      echo "MISSING FROM SWIFT ($component; $missing_swift concept(s)):"
      report_words "$component" swift "$dir/missing-swift" "$dir/rust-map"
      echo
    fi
    if (( missing_kotlin > 0 )); then
      echo "MISSING FROM KOTLIN ($component; $missing_kotlin concept(s)):"
      report_words "$component" kotlin "$dir/missing-kotlin" "$dir/rust-map"
      echo
    fi
    if (( allowed_swift > 0 )); then
      echo "ALLOWLISTED FOR SWIFT ($component; $allowed_swift concept(s)):"
      report_words "$component" swift "$dir/allowed-swift" "$dir/rust-map"
      echo
    fi
    if (( allowed_kotlin > 0 )); then
      echo "ALLOWLISTED FOR KOTLIN ($component; $allowed_kotlin concept(s)):"
      report_words "$component" kotlin "$dir/allowed-kotlin" "$dir/rust-map"
      echo
    fi
    if (( stale_swift > 0 )); then
      echo "CURRENTLY-UNUSED ALLOWLIST ENTRIES FOR SWIFT ($component):"
      sed 's/^/  - /' "$dir/stale-swift"
      echo
    fi
    if (( stale_kotlin > 0 )); then
      echo "CURRENTLY-UNUSED ALLOWLIST ENTRIES FOR KOTLIN ($component):"
      sed 's/^/  - /' "$dir/stale-kotlin"
      echo
    fi
  fi
done

if [[ $QUIET -eq 1 ]]; then
  echo "check-sdk-parity: components=$total_components rust-concepts=$total_rust_words missing-from-swift=$total_missing_swift missing-from-kotlin=$total_missing_kotlin allowlisted-swift=$total_allowed_swift allowlisted-kotlin=$total_allowed_kotlin stale-allowlist-swift=$total_stale_swift stale-allowlist-kotlin=$total_stale_kotlin"
elif (( total_missing_swift == 0 && total_missing_kotlin == 0 )); then
  echo "OK: $total_components component(s) have per-component Swift/Kotlin concept coverage outside exact TOML exceptions."
fi

if (( total_missing_swift > 0 || total_missing_kotlin > 0 )); then
  exit 1
fi
