#!/usr/bin/env bash
#
# check-sdk-parity.sh -- mechanical Cross-SDK Parity check (issue #496, gate 5).
#
# WHY
#   #493 shipped a Rust/UniFFI NIP-02 "Following" surface with a full Swift
#   projection and *zero* Kotlin projection, silently breaking the parity the
#   docs claim between the three surfaces. Nothing in CI would have caught
#   that: `scripts/check-surface-governance.sh` only requires a changelog
#   entry when swift/kotlin/rust/ffi paths change in the *same* PR -- it does
#   not compare what each side actually exposes. This script does the
#   complementary, narrower thing: it diffs the public *vocabulary* of the
#   Rust FFI export surface against what the Swift and Kotlin SDKs actually
#   contain, and fails when one side has concepts the other lacks entirely.
#
# HOW IT DECIDES "PUBLIC SURFACE" (read this before trusting a red run)
#   Rust side (crates/nmp-ffi/src/*.rs):
#     - Every `#[derive(... Object|Enum|Record|Error ...)]` (uniffi-prefixed
#       or bare, e.g. `use uniffi::{Enum, Record}; #[derive(.., Enum)]`)
#       immediately followed by a `struct`/`enum` declaration contributes
#       that type's name.
#     - Every `#[uniffi::export]` / `#[uniffi::export(callback_interface)]`
#       block (a free `fn`, an `impl` block, or a callback `trait`)
#       contributes every `fn NAME` found before that block's closing brace
#       in column 0. This is a plain state machine over source text, not a
#       real Rust parser.
#   Both type and function/method names are tokenized (CamelCase and
#   snake_case split into lowercase words, e.g. `FfiFollowSnapshot` and
#   `observe_following` both yield "follow"/"following"/"snapshot"), a
#   stopword list strips connective/structural noise (new, get, set, ffi,
#   nmp, pub, self, ok, err, ...), and words shorter than 3 characters are
#   dropped. What remains is treated as the set of *concepts* the Rust FFI
#   surface exposes.
#
#   Swift/Kotlin side:
#     - Every identifier-shaped token in every `.swift` file under
#       `Packages/NMP/Sources/**` (all targets: NMP, NMPContent, NMPUI -- a
#       feature can legitimately live in a separate Swift target) and every
#       `.kt` file under `Packages/NMPKotlin/src/main/kotlin/**` is
#       tokenized the same way into a word set.
#
#   A Rust concept word that does not appear anywhere in the Swift word set
#   (or the Kotlin word set) is reported as missing on that side.
#
# KNOWN LIMITATIONS (this is a heuristic text scan, not semantic diffing)
#   - Word-level, not symbol-level: it proves a *concept* (e.g. "following")
#     is unmentioned anywhere on one side, not that every method/field lines
#     up 1:1. It will not catch a subtly wrong signature or a renamed-but-
#     present field.
#   - False positives are possible for Rust-only plumbing words that never
#     need a language-parallel identifier (e.g. an internal getter named
#     after a generic verb). Review the reported example symbol before
#     treating a line as an action item.
#   - False negatives are possible if a word only appears in an unrelated
#     comment, string, or coincidentally-named-but-unrelated identifier on
#     the "present" side.
#   - Multi-line `#[derive(...)]` attributes (attribute split across
#     several lines) are not recognized; every current usage in this repo is
#     single-line, but a future multi-line derive would silently drop that
#     type from the Rust concept set.
#   - This script has no cargo/gradle/swift toolchain dependency on purpose
#     (shell + grep/sed/awk/comm only) so it can run in any lane, including
#     ones that must not invoke cargo.
#
# USAGE
#   scripts/check-sdk-parity.sh            # human-readable report
#   scripts/check-sdk-parity.sh --quiet    # summary counts only
#
# EXIT STATUS
#   0  Rust FFI concept words are all represented in both Swift and Kotlin.
#   1  At least one concept word is missing from Swift and/or Kotlin.
#
set -u

QUIET=0
if [[ "${1:-}" == "--quiet" ]]; then
  QUIET=1
fi

REPO_ROOT="$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel 2>/dev/null || true)"
if [[ -z "$REPO_ROOT" ]]; then
  echo "check-sdk-parity: could not resolve repo root (not inside a git checkout?)" >&2
  exit 2
fi

FFI_DIR="$REPO_ROOT/crates/nmp-ffi/src"
SWIFT_DIR="$REPO_ROOT/Packages/NMP/Sources"
KOTLIN_DIR="$REPO_ROOT/Packages/NMPKotlin/src/main/kotlin"

for d in "$FFI_DIR" "$SWIFT_DIR" "$KOTLIN_DIR"; do
  if [[ ! -d "$d" ]]; then
    echo "check-sdk-parity: expected directory missing: $d" >&2
    echo "check-sdk-parity: the repo layout has moved; update FFI_DIR/SWIFT_DIR/KOTLIN_DIR at the top of this script." >&2
    exit 2
  fi
done

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t nmp-sdk-parity)
trap 'rm -rf "$tmp"' EXIT

# Connective/structural words that are never, by themselves, an interesting
# cross-language concept. Kept short and reviewable -- extend deliberately.
STOPWORDS_RE='^(new|get|set|default|project|op|inner|cancel|disconnect|connect|self|id|ids|ref|mut|arc|box|dyn|into|from|with|and|the|is|as|ok|err|some|none|true|false|str|string|u8|u16|u32|u64|i8|i16|i32|i64|f32|f64|bool|vec|option|result|ffi|nmp|fn|pub|impl|struct|enum|trait|type|let|var|func|class|public|private|internal|open|override|companion|data|sealed|typealias|import|package|extension|protocol|static|on|to|of|in|for|by|it|an|com|sdk|kotlin|swift|clone|debug|eq|partialeq|hash|hashable|sendable|codable|copy|not|test|tests|mod)$'

# Split CamelCase and snake_case identifiers into lowercase word lines,
# drop punctuation/digits-only noise, apply the stopword list, and require
# at least 3 characters so single/double-letter fragments don't pollute
# the report.
tokenize() {
  sed -E 's/([a-z0-9])([A-Z])/\1_\2/g; s/([A-Z]+)([A-Z][a-z])/\1_\2/g' \
    | tr '[:upper:]' '[:lower:]' \
    | tr -cs 'a-z0-9' '_' \
    | tr '_' '\n' \
    | grep -v '^$' \
    | grep -vE "$STOPWORDS_RE" \
    | awk 'length($0) >= 3'
}

# --- 1. Rust exported type names: Object/Enum/Record/Error derives ---------
: > "$tmp/rust_types.txt"
for f in "$FFI_DIR"/*.rs; do
  grep -A1 -E '#\[derive\(.*\b(Object|Enum|Record|Error)\b.*\)\]' "$f" 2>/dev/null \
    | grep -E '^(pub )?(struct|enum) ' \
    | sed -E 's/^(pub )?(struct|enum)[ \t]+([A-Za-z0-9_]+).*/\3/' >> "$tmp/rust_types.txt"
done

# --- 2. Rust exported fn/method names: state machine over export blocks ---
# An export block is opened by a `#[uniffi::export...]` attribute line and
# closed by the next column-0 `}` (the enclosing `impl`/`fn`/`trait`'s own
# closing brace under standard rustfmt indentation). Every `fn NAME` seen
# while "inside" such a block is captured, whether it is a free function, an
# impl method, or a callback-interface trait method.
: > "$tmp/rust_fns.txt"
for f in "$FFI_DIR"/*.rs; do
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
    if [[ $capturing -eq 1 ]]; then
      if [[ "$line" =~ (^|[^A-Za-z_])(pub[[:space:]]+)?fn[[:space:]]+([A-Za-z_][A-Za-z0-9_]*) ]]; then
        echo "${BASH_REMATCH[3]}" >> "$tmp/rust_fns.txt"
      fi
    fi
  done < "$f"
done

cat "$tmp/rust_types.txt" "$tmp/rust_fns.txt" | sort -u > "$tmp/rust_symbols.txt"

if [[ ! -s "$tmp/rust_symbols.txt" ]]; then
  echo "check-sdk-parity: extracted zero Rust FFI export symbols from $FFI_DIR -- the extraction heuristic likely broke against a repo layout/style change. Treating this as a hard failure rather than silently passing." >&2
  exit 2
fi

# word -> example originating Rust symbol (first one wins), for readable output
: > "$tmp/rust_word_symbol.txt"
while IFS= read -r sym; do
  [[ -z "$sym" ]] && continue
  printf '%s\n' "$sym" | tokenize | while IFS= read -r w; do
    printf '%s\t%s\n' "$w" "$sym" >> "$tmp/rust_word_symbol.txt"
  done
done < "$tmp/rust_symbols.txt"

sort -t "$(printf '\t')" -k1,1 -u "$tmp/rust_word_symbol.txt" > "$tmp/rust_word_symbol_sorted.txt"
cut -f1 "$tmp/rust_word_symbol_sorted.txt" | sort -u > "$tmp/rust_words.txt"

# --- 3. Swift / Kotlin word sets -------------------------------------------
extract_words() {
  local dir="$1" ext="$2"
  find "$dir" -type f -name "*.$ext" -print0 2>/dev/null \
    | xargs -0 grep -hEo '[A-Za-z_][A-Za-z0-9_]*' 2>/dev/null \
    | tokenize \
    | sort -u
}

extract_words "$SWIFT_DIR" swift > "$tmp/swift_words.txt"
extract_words "$KOTLIN_DIR" kt > "$tmp/kotlin_words.txt"

comm -23 "$tmp/rust_words.txt" "$tmp/swift_words.txt" > "$tmp/missing_swift.txt"
comm -23 "$tmp/rust_words.txt" "$tmp/kotlin_words.txt" > "$tmp/missing_kotlin.txt"

missing_swift_count=$(wc -l < "$tmp/missing_swift.txt" | tr -d ' ')
missing_kotlin_count=$(wc -l < "$tmp/missing_kotlin.txt" | tr -d ' ')

report_side() {
  local label="$1" file="$2"
  while IFS= read -r w; do
    [[ -z "$w" ]] && continue
    example=$(awk -F '\t' -v w="$w" '$1 == w { print $2; exit }' "$tmp/rust_word_symbol_sorted.txt")
    printf '  - %-20s (e.g. Rust FFI symbol: %s)\n' "$w" "$example"
  done < "$file"
}

if [[ $QUIET -eq 0 ]]; then
  echo "check-sdk-parity: Rust FFI export concept words: $(wc -l < "$tmp/rust_words.txt" | tr -d ' ')"
  echo
  if [[ $missing_swift_count -gt 0 ]]; then
    echo "MISSING FROM SWIFT ($missing_swift_count concept word(s) present in the Rust FFI surface but absent from Packages/NMP/Sources/**):"
    report_side swift "$tmp/missing_swift.txt"
    echo
  fi
  if [[ $missing_kotlin_count -gt 0 ]]; then
    echo "MISSING FROM KOTLIN ($missing_kotlin_count concept word(s) present in the Rust FFI surface but absent from Packages/NMPKotlin/src/main/kotlin/**):"
    report_side kotlin "$tmp/missing_kotlin.txt"
    echo
  fi
  if [[ $missing_swift_count -eq 0 && $missing_kotlin_count -eq 0 ]]; then
    echo "OK: no Rust FFI concept word is entirely absent from Swift or Kotlin."
  fi
else
  echo "check-sdk-parity: missing-from-swift=$missing_swift_count missing-from-kotlin=$missing_kotlin_count"
fi

if [[ $missing_swift_count -gt 0 || $missing_kotlin_count -gt 0 ]]; then
  exit 1
fi
exit 0
