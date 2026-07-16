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
#   Rust side (every .rs file under crates/nmp-ffi/src/, recursively):
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
#     - The generated UniFFI bindings (`Packages/NMP/Sources/NMPFFI/**` and
#       `Packages/NMPKotlin/src/main/kotlin/uniffi/**`) are EXCLUDED. They
#       are gitignored build artifacts that by construction contain every
#       Rust FFI symbol name; a locally-built tree that included them would
#       make this whole check vacuously green (CI checkouts never contain
#       them, so including them would also make local runs disagree with CI).
#       Only the hand-written SDK surface counts as "present".
#
#   A Rust concept word that does not appear anywhere in the Swift word set
#   (or the Kotlin word set) is reported as missing on that side.
#
# ALLOWLIST (documented per-platform modeling exceptions)
#   `scripts/check-sdk-parity-allowlist.txt` may suppress exactly one concept
#   word on exactly one side per entry, each with a one-line justification.
#   Suppressed words are still printed (as allowlisted) so they stay visible;
#   entries that currently suppress nothing are reported so they cannot rot
#   silently (an entry may deliberately outlive incidental word presence --
#   its justification line says which). A malformed allowlist line is a hard
#   error -- the escape hatch stays narrow.
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
#   0  Rust FFI concept words are all represented in both Swift and Kotlin
#      (allowlisted exceptions excluded).
#   1  At least one non-allowlisted concept word is missing from Swift and/or
#      Kotlin.
#   2  The script itself could not run soundly (layout moved, extraction
#      broke, malformed allowlist).
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
ALLOWLIST_FILE="$REPO_ROOT/scripts/check-sdk-parity-allowlist.txt"

# Generated UniFFI bindings: gitignored build artifacts that contain every FFI
# symbol by construction. Never count them as SDK surface (see header).
SWIFT_GENERATED_EXCLUDE="*/Sources/NMPFFI/*"
KOTLIN_GENERATED_EXCLUDE="*/kotlin/uniffi/*"

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
# Recursive on purpose: if FFI exports move into src/ subdirectories, a
# top-level-only glob would silently drop them and this check would go
# vacuously green.
find "$FFI_DIR" -type f -name '*.rs' | LC_ALL=C sort > "$tmp/ffi_files.txt"
if [[ ! -s "$tmp/ffi_files.txt" ]]; then
  echo "check-sdk-parity: found zero .rs files under $FFI_DIR -- the repo layout has moved; update FFI_DIR at the top of this script." >&2
  exit 2
fi

: > "$tmp/rust_types.txt"
while IFS= read -r f; do
  grep -A1 -E '#\[derive\(.*\b(Object|Enum|Record|Error)\b.*\)\]' "$f" 2>/dev/null \
    | grep -E '^(pub )?(struct|enum) ' \
    | sed -E 's/^(pub )?(struct|enum)[ \t]+([A-Za-z0-9_]+).*/\3/' >> "$tmp/rust_types.txt"
done < "$tmp/ffi_files.txt"

# --- 2. Rust exported fn/method names: state machine over export blocks ---
# An export block is opened by a `#[uniffi::export...]` attribute line and
# closed by the next column-0 `}` (the enclosing `impl`/`fn`/`trait`'s own
# closing brace under standard rustfmt indentation). Every `fn NAME` seen
# while "inside" such a block is captured, whether it is a free function, an
# impl method, or a callback-interface trait method.
: > "$tmp/rust_fns.txt"
while IFS= read -r f; do
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
done < "$tmp/ffi_files.txt"

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
  local dir="$1" ext="$2" exclude="$3"
  find "$dir" -type f -name "*.$ext" -not -path "$exclude" -print0 2>/dev/null \
    | xargs -0 grep -hEo '[A-Za-z_][A-Za-z0-9_]*' 2>/dev/null \
    | tokenize \
    | sort -u
}

extract_words "$SWIFT_DIR" swift "$SWIFT_GENERATED_EXCLUDE" > "$tmp/swift_words.txt"
extract_words "$KOTLIN_DIR" kt "$KOTLIN_GENERATED_EXCLUDE" > "$tmp/kotlin_words.txt"

for side in swift kotlin; do
  if [[ ! -s "$tmp/${side}_words.txt" ]]; then
    echo "check-sdk-parity: extracted zero $side identifier words -- the repo layout or exclusion pattern likely broke. Treating this as a hard failure rather than reporting everything missing." >&2
    exit 2
  fi
done

comm -23 "$tmp/rust_words.txt" "$tmp/swift_words.txt" > "$tmp/missing_swift_raw.txt"
comm -23 "$tmp/rust_words.txt" "$tmp/kotlin_words.txt" > "$tmp/missing_kotlin_raw.txt"

# --- 4. Allowlist: documented per-platform modeling exceptions -------------
: > "$tmp/allow_swift.txt"
: > "$tmp/allow_kotlin.txt"
if [[ -f "$ALLOWLIST_FILE" ]]; then
  lineno=0
  while IFS= read -r line || [[ -n "$line" ]]; do
    lineno=$((lineno + 1))
    [[ -z "${line//[[:space:]]/}" || "$line" == \#* ]] && continue
    word=$(printf '%s\n' "$line" | awk '{print $1}')
    side=$(printf '%s\n' "$line" | awk '{print $2}')
    just=$(printf '%s\n' "$line" | awk '{$1=""; $2=""; sub(/^  */,""); print}')
    if [[ ! "$word" =~ ^[a-z0-9]{3,}$ || ! "$side" =~ ^(swift|kotlin)$ || -z "${just//[[:space:]]/}" ]]; then
      echo "check-sdk-parity: malformed allowlist line $lineno in $ALLOWLIST_FILE (need: <lowercase-word> <swift|kotlin> <justification>): $line" >&2
      exit 2
    fi
    printf '%s\n' "$word" >> "$tmp/allow_$side.txt"
  done < "$ALLOWLIST_FILE"
  sort -u "$tmp/allow_swift.txt" -o "$tmp/allow_swift.txt"
  sort -u "$tmp/allow_kotlin.txt" -o "$tmp/allow_kotlin.txt"
fi

# Split each raw missing list into allowlisted (visible, non-failing) and
# real (failing); report allowlist entries that suppress nothing as stale.
comm -23 "$tmp/missing_swift_raw.txt" "$tmp/allow_swift.txt" > "$tmp/missing_swift.txt"
comm -12 "$tmp/missing_swift_raw.txt" "$tmp/allow_swift.txt" > "$tmp/allowed_swift.txt"
comm -13 "$tmp/missing_swift_raw.txt" "$tmp/allow_swift.txt" > "$tmp/stale_allow_swift.txt"
comm -23 "$tmp/missing_kotlin_raw.txt" "$tmp/allow_kotlin.txt" > "$tmp/missing_kotlin.txt"
comm -12 "$tmp/missing_kotlin_raw.txt" "$tmp/allow_kotlin.txt" > "$tmp/allowed_kotlin.txt"
comm -13 "$tmp/missing_kotlin_raw.txt" "$tmp/allow_kotlin.txt" > "$tmp/stale_allow_kotlin.txt"

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

report_allowlisted() {
  local label="$1" file="$2"
  if [[ -s "$file" ]]; then
    echo "ALLOWLISTED FOR $label ($(wc -l < "$file" | tr -d ' ') word(s) absent but documented in scripts/check-sdk-parity-allowlist.txt -- not failing):"
    report_side "$label" "$file"
    echo
  fi
}

report_stale() {
  local label="$1" file="$2"
  if [[ -s "$file" ]]; then
    echo "CURRENTLY-UNUSED ALLOWLIST ENTRIES FOR $label (word is present on that side today, so the entry suppresses nothing; check scripts/check-sdk-parity-allowlist.txt -- delete the entry if the SDK now genuinely covers the concept, keep it if the entry says presence is incidental):"
    sed 's/^/  - /' "$file"
    echo
  fi
}

if [[ $QUIET -eq 0 ]]; then
  echo "check-sdk-parity: Rust FFI export concept words: $(wc -l < "$tmp/rust_words.txt" | tr -d ' ')"
  echo
  if [[ $missing_swift_count -gt 0 ]]; then
    echo "MISSING FROM SWIFT ($missing_swift_count concept word(s) present in the Rust FFI surface but absent from hand-written Packages/NMP/Sources/**):"
    report_side swift "$tmp/missing_swift.txt"
    echo
  fi
  if [[ $missing_kotlin_count -gt 0 ]]; then
    echo "MISSING FROM KOTLIN ($missing_kotlin_count concept word(s) present in the Rust FFI surface but absent from hand-written Packages/NMPKotlin/src/main/kotlin/**):"
    report_side kotlin "$tmp/missing_kotlin.txt"
    echo
  fi
  report_allowlisted swift "$tmp/allowed_swift.txt"
  report_allowlisted kotlin "$tmp/allowed_kotlin.txt"
  report_stale swift "$tmp/stale_allow_swift.txt"
  report_stale kotlin "$tmp/stale_allow_kotlin.txt"
  if [[ $missing_swift_count -eq 0 && $missing_kotlin_count -eq 0 ]]; then
    echo "OK: no Rust FFI concept word is entirely absent from Swift or Kotlin (outside documented allowlist entries)."
  fi
else
  echo "check-sdk-parity: missing-from-swift=$missing_swift_count missing-from-kotlin=$missing_kotlin_count allowlisted-swift=$(wc -l < "$tmp/allowed_swift.txt" | tr -d ' ') allowlisted-kotlin=$(wc -l < "$tmp/allowed_kotlin.txt" | tr -d ' ')"
fi

if [[ $missing_swift_count -gt 0 || $missing_kotlin_count -gt 0 ]]; then
  exit 1
fi
exit 0
