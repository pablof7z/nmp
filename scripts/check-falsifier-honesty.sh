#!/usr/bin/env bash
#
# check-falsifier-honesty.sh -- mechanical Falsifier-honesty check
# (issue #496, gate 6; docs/design/architecture-review-gates.md).
#
# WHY
#   docs/bug-class-ledger.md:3-5 makes proof-by-falsifier the closing move of
#   every bug class, and the ledger's closing rule says "a design document,
#   issue, or passing adjacent test is not proof." A PR's own prose is not
#   proof either: a "Updated falsifiers" field or a "Falsifiers" section that
#   names a mechanism the diff never actually adds tells the next reader a
#   bad path is excluded when it is not. This script makes the cheapest
#   version of that lie mechanical to catch: every concretely-named symbol a
#   PR's falsifier claims cite must actually exist in the tree the PR
#   produces.
#
# WHAT IT CHECKS (deliberately narrow -- robust to false positives)
#   Claim sources:
#     1. Lines ADDED to docs/surface-change-log.md between BASE and HEAD,
#        restricted to each appended entry's "Updated falsifiers:" field
#        (from the field marker up to the next "- **Field:**" or "## "
#        heading, so wrapped field values are covered).
#     2. Optionally, a claims file (e.g. the PR body dumped by CI), restricted
#        to (a) markdown sections whose heading contains "falsifier"
#        (case-insensitive) and (b) individual lines matching
#        "falsifiers?...:" (e.g. "**Updated falsifiers:** ...").
#   From those regions ONLY backtick code spans are considered, and a span
#   only becomes a checkable claim when it parses cleanly as ONE symbol or
#   ONE source path:
#     - `path/to/file.rs` (or .swift/.kt/.kts/.sh/.py/.toml/.yml/.yaml with a
#       "/"): the exact path must exist in the HEAD tree; a bare filename
#       (`Row.swift`) must exist as some tracked file's basename.
#     - `some_symbol`, `Some::Path::symbol`, `CamelCaseType`,
#       `.swiftEnumCase`, `fn_name()`: normalized to its last path component;
#       it must then contain "_" or mixed case and be >= 4 chars (this drops
#       prose words like `acquire` or `AUTH`). The symbol must appear, as a
#       whole word, somewhere in the HEAD tree outside markdown/docs -- i.e.
#       in code, tests, or scripts, not merely in prose about itself.
#   Anything else inside backticks (commands with spaces, attribute soup,
#   arrows, brace lists) is skipped as unverifiable prose, never failed.
#
# WHAT IT DOES NOT CHECK
#   - That a named falsifier actually falsifies anything, runs in CI, or was
#     modified by this PR (a claim may legitimately cite a pre-existing
#     falsifier that already covers the change). Presence is the floor;
#     review still owns the semantics.
#   - Whole-word presence is a text match over file content: it cannot
#     distinguish a real definition from a same-named identifier in an
#     unrelated file, and it IS satisfied by the bare word appearing in a
#     source-code comment or dead code (a deliberately dishonest PR could
#     plant `// StoreStillOpen` in any .rs file). Such a plant is visible in
#     the same diff a reviewer reads; gate 6's by-eye half still applies.
#     The check CAN never be satisfied by the claim text itself, because
#     markdown is excluded from the search.
#
# USAGE
#   scripts/check-falsifier-honesty.sh BASE_REF HEAD_REF [CLAIMS_FILE]
#     BASE_REF     the PR base (e.g. origin/master or the PR base SHA); the
#                  script diffs from `git merge-base BASE_REF HEAD_REF`, so a
#                  base branch that moved after the PR branched cannot leak
#                  other PRs' change-log entries into this PR's claims
#     HEAD_REF     the PR head (or merge) commit to verify against
#     CLAIMS_FILE  optional file containing extra claim prose (PR body)
#
# EXIT STATUS
#   0  every checkable named mechanism exists at HEAD_REF (including the
#      "no claims found" case: making no claim is honest)
#   1  at least one named mechanism is absent from the HEAD tree
#   2  the script could not run soundly (bad refs, usage error)
set -u

if [[ $# -lt 2 || $# -gt 3 ]]; then
  echo "usage: $0 BASE_REF HEAD_REF [CLAIMS_FILE]" >&2
  exit 2
fi
BASE_REF="$1"
HEAD_REF="$2"
CLAIMS_FILE="${3:-}"

REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null || true)"
if [[ -z "$REPO_ROOT" ]]; then
  echo "check-falsifier-honesty: not inside a git checkout" >&2
  exit 2
fi
cd "$REPO_ROOT"

for ref in "$BASE_REF" "$HEAD_REF"; do
  if ! git rev-parse --verify --quiet "$ref^{commit}" > /dev/null; then
    echo "check-falsifier-honesty: not a resolvable commit: $ref" >&2
    exit 2
  fi
done

# Scope the change-log diff to lines this PR actually added: diff from the
# merge base, not from a base tip that may have moved since branching.
MERGE_BASE=$(git merge-base "$BASE_REF" "$HEAD_REF" 2>/dev/null || true)
if [[ -z "$MERGE_BASE" ]]; then
  echo "check-falsifier-honesty: no merge base between $BASE_REF and $HEAD_REF" >&2
  exit 2
fi

CHANGE_LOG="docs/surface-change-log.md"

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t nmp-falsifier-honesty)
trap 'rm -rf "$tmp"' EXIT

# --- 1. Claim text from appended change-log "Updated falsifiers" fields ----
# Only lines ADDED by this PR ("+" diff lines), and only from the falsifiers
# field marker until the next field marker or entry heading.
git diff "$MERGE_BASE..$HEAD_REF" -- "$CHANGE_LOG" 2>/dev/null \
  | grep -E '^\+' | grep -vE '^\+\+\+' | sed 's/^\+//' \
  | awk '
      /^- \*\*Updated falsifiers:\*\*/ { capture = 1 }
      capture && !/^- \*\*Updated falsifiers:\*\*/ && (/^- \*\*/ || /^## /) { capture = 0 }
      capture { print }
    ' > "$tmp/claims_changelog.txt"

# --- 2. Claim text from the optional claims file (PR body) -----------------
: > "$tmp/claims_body.txt"
if [[ -n "$CLAIMS_FILE" ]]; then
  if [[ ! -f "$CLAIMS_FILE" ]]; then
    echo "check-falsifier-honesty: claims file not found: $CLAIMS_FILE" >&2
    exit 2
  fi
  # (a) markdown sections whose heading mentions falsifier(s).
  #
  # `^#+ ` and NOT `^#{1,6} `. Interval expressions are an ERE extension that
  # mawk does not implement, and mawk is /usr/bin/awk on Debian and on the
  # ubuntu-latest runners this gate runs on. There, `/^#{1,6} /` matches the
  # LITERAL text `#{1,6} ` and therefore nothing -- so this entire claim
  # source silently produced no claims in CI, for every PR, since it was
  # written. `#+` needs no intervals and is exact here: markdown caps headings
  # at six `#` and a 7+ run is not a heading anyway.
  awk '
      /^#+ /           { in_section = ($0 ~ /[Ff]alsifier/) }
      in_section       { print }
    ' "$CLAIMS_FILE" > "$tmp/claims_body.txt"
  # (b) single lines that label a falsifier list inline
  grep -iE '(^|[*_[:space:]])falsifiers?[^`]*:' "$CLAIMS_FILE" >> "$tmp/claims_body.txt" || true
fi

cat "$tmp/claims_changelog.txt" "$tmp/claims_body.txt" > "$tmp/claims.txt"

if [[ ! -s "$tmp/claims.txt" ]]; then
  echo "check-falsifier-honesty: no falsifier claims found in appended $CHANGE_LOG entries${CLAIMS_FILE:+ or $CLAIMS_FILE}; nothing to verify (making no claim is honest)."
  exit 0
fi

# --- 3. Extract checkable claims from backtick spans -----------------------
grep -oE '`[^`]+`' "$tmp/claims.txt" 2>/dev/null \
  | sed 's/^`//; s/`$//' | sort -u > "$tmp/spans.txt"

PATH_EXT_RE='\.(rs|swift|kt|kts|sh|py|toml|yml|yaml)$'
SYMBOL_RE='^[A-Za-z_][A-Za-z0-9_]*(::[A-Za-z_][A-Za-z0-9_]*)*$'

: > "$tmp/path_claims.txt"
: > "$tmp/symbol_claims.txt"   # lines: <symbol-to-find>\t<original-span>
: > "$tmp/unrecognised.txt"    # claim-shaped spans that parsed as neither

# Is this span shaped like a claim someone MEANT to be checked? Used only to
# decide whether falling through both parsers is worth reporting: prose spans
# (`--release`, `Arc<T>`, `#[derive]`) are legitimately skipped and must stay
# silent, but a span carrying a source extension or a `::` path was written as
# a claim, and silently checking nothing is the #950 failure.
looks_like_claim() {
  [[ "$1" =~ \.(rs|swift|kt|kts|sh|py|toml|yml|yaml)($|::|:) ]] && return 0
  [[ "$1" =~ ^[A-Za-z0-9_./-]+::[A-Za-z_] ]] && return 0
  return 1
}

while IFS= read -r span; do
  [[ -z "$span" ]] && continue
  # spans with whitespace are commands/prose, not a single named mechanism
  [[ "$span" =~ [[:space:]] ]] && continue
  token="$span"
  # Strip a leading `.` ONLY for `.swiftEnumCase`. A token containing `/` is a
  # path, and `.github/workflows/ci.yml` losing its dot became
  # `github/workflows/ci.yml`, which the `(^|/)` anchor can never match
  # against the real tracked path -- reporting NAMED-BUT-ABSENT for a file
  # that exists (#946, #928).
  [[ "$token" != */* ]] && token="${token#.}"
  token="${token%%(*}"                  # fn_name(...) / Case(assoc:) -> name
  token="${token%!}"                    # macro_name! -> macro_name
  token="${token%,}"; token="${token%;}"; token="${token%.}"
  [[ -z "$token" ]] && continue

  # `path/to/file.rs::symbol` and `path/to/file.rs:123` -- the forms people
  # actually write. Previously BOTH regexes rejected these (the `/` fails
  # SYMBOL_RE; the trailing symbol/line fails PATH_EXT_RE's `$` anchor), so
  # they fell through and were checked by NOTHING, silently. On PR #907 that
  # skipped all three Rust falsifiers carrying the exact-bytes proof while the
  # gate reported green (#950).
  if [[ "$token" =~ ^([A-Za-z0-9_./-]+\.(rs|swift|kt|kts|sh|py|toml|yml|yaml))(::(.+)|:[0-9]+)$ ]]; then
    printf '%s\t%s\n' "${BASH_REMATCH[1]}" "$span" >> "$tmp/path_claims.txt"
    qualified_symbol="${BASH_REMATCH[4]:-}"
    if [[ -n "$qualified_symbol" ]]; then
      leaf="${qualified_symbol##*::}"
      leaf="${leaf%%(*}"
      # The leaf must be a PLAIN identifier. Without this, a span like
      # `foo.rs::sym::extra<Generic>` yields the leaf `extra<Generic>`, which
      # `git grep -w` can never match, turning a parser gap into a false
      # NAMED-BUT-ABSENT against the author.
      if [[ ! "$leaf" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]]; then
        printf '%s\n' "$span" >> "$tmp/unrecognised.txt"
      elif [[ ${#leaf} -ge 4 ]] \
         && { [[ "$leaf" == *_* ]] || { [[ "$leaf" =~ [a-z] && "$leaf" =~ [A-Z] ]]; }; }; then
        printf '%s\t%s\n' "$leaf" "$span" >> "$tmp/symbol_claims.txt"
      fi
    fi
    continue
  fi

  if [[ "$token" =~ $PATH_EXT_RE && "$token" =~ ^[A-Za-z0-9_./-]+$ ]]; then
    printf '%s\t%s\n' "$token" "$span" >> "$tmp/path_claims.txt"
    continue
  fi
  if [[ "$token" =~ $SYMBOL_RE ]]; then
    leaf="${token##*::}"
    # concrete-symbol shape: snake_case or mixed case, >= 4 chars. Plain
    # lowercase words and ALL-CAPS words are prose, not symbol claims.
    if [[ ${#leaf} -ge 4 ]] \
       && { [[ "$leaf" == *_* ]] || { [[ "$leaf" =~ [a-z] && "$leaf" =~ [A-Z] ]]; }; }; then
      printf '%s\t%s\n' "$leaf" "$span" >> "$tmp/symbol_claims.txt"
    fi
    continue
  fi
  looks_like_claim "$token" && printf '%s\n' "$span" >> "$tmp/unrecognised.txt"
done < "$tmp/spans.txt"

sort -u "$tmp/unrecognised.txt" -o "$tmp/unrecognised.txt"

sort -u "$tmp/path_claims.txt" -o "$tmp/path_claims.txt"
sort -u "$tmp/symbol_claims.txt" -o "$tmp/symbol_claims.txt"

path_total=$(wc -l < "$tmp/path_claims.txt" | tr -d ' ')
symbol_total=$(wc -l < "$tmp/symbol_claims.txt" | tr -d ' ')
unrecognised_total=$(wc -l < "$tmp/unrecognised.txt" | tr -d ' ')

# Report claim-shaped spans this script could not parse. Silence here is the
# #950 defect itself: an author writing a falsifier in a format the parsers
# miss got a green tick having been checked on nothing, with no signal that it
# had happened. This is deliberately a REPORT and not a failure -- a parser
# gap is the gate's bug, not the author's, and failing the PR would punish the
# wrong party. What it must not do is stay quiet.
if [[ $unrecognised_total -gt 0 ]]; then
  echo "check-falsifier-honesty: NOT CHECKED -- $unrecognised_total claim-shaped span(s) matched no parser:"
  sed 's/^/    /' "$tmp/unrecognised.txt"
  echo "    (these were counted as claims by nobody; if one names a real falsifier, the parser needs widening -- please report it)"
  echo
fi

if [[ $path_total -eq 0 && $symbol_total -eq 0 ]]; then
  echo "check-falsifier-honesty: falsifier prose found, but no concretely-named symbol/path claims to verify (only checkable claims are enforced)."
  exit 0
fi

# --- 4. Verify each claim against the HEAD tree ----------------------------
git ls-tree -r --name-only "$HEAD_REF" > "$tmp/head_files.txt"

failures=0
checked=0

while IFS=$'\t' read -r path span; do
  [[ -z "$path" ]] && continue
  checked=$((checked + 1))
  if [[ "$path" == */* ]]; then
    # Exact tracked path, or a crate/package-relative suffix of one
    # (change-log prose often writes `tests/foo.rs` next to the crate name).
    path_re=$(printf '%s' "$path" | sed 's/[.[\*^$+?{}()|]/\\&/g')
    grep -Eq "(^|/)$path_re$" "$tmp/head_files.txt" && continue
  else
    grep -Eq "(^|/)$(printf '%s' "$path" | sed 's/[.[\*^$+?{}()|]/\\&/g')$" "$tmp/head_files.txt" && continue
  fi
  echo "NAMED-BUT-ABSENT path: \`$span\` is claimed as a falsifier location but no such file exists at $HEAD_REF"
  failures=$((failures + 1))
done < "$tmp/path_claims.txt"

while IFS=$'\t' read -r leaf span; do
  [[ -z "$leaf" ]] && continue
  checked=$((checked + 1))
  # Whole-word presence anywhere in the HEAD tree except markdown/prose.
  # Markdown is excluded so a claim can never satisfy itself.
  if git grep -I -F -w --quiet -e "$leaf" "$HEAD_REF" -- ':(exclude)*.md' ':(exclude)docs/'; then
    continue
  fi
  # A falsifier is often cited by its integration-test FILE name
  # (`relay_ingest_smoke` -> crates/.../tests/relay_ingest_smoke.rs), which
  # never appears inside file content. A tracked non-markdown file whose stem
  # is exactly the symbol also satisfies the claim.
  leaf_re=$(printf '%s' "$leaf" | sed 's/[.[\*^$+?{}()|]/\\&/g')
  if grep -E "(^|/)$leaf_re\.[A-Za-z0-9]+$" "$tmp/head_files.txt" | grep -vE '\.md$' | grep -q .; then
    continue
  fi
  echo "NAMED-BUT-ABSENT mechanism: \`$span\` is claimed as a falsifier/mechanism but the symbol \`$leaf\` appears nowhere in the $HEAD_REF tree outside markdown"
  failures=$((failures + 1))
done < "$tmp/symbol_claims.txt"

echo
echo "check-falsifier-honesty: checked $checked concretely-named claim(s) ($symbol_total symbol(s), $path_total path(s)) against $HEAD_REF: $failures absent."

if [[ $failures -gt 0 ]]; then
  echo "check-falsifier-honesty: FAIL -- a falsifier/mechanism this PR names does not exist in the tree it produces. Either land the mechanism or stop claiming it (docs/design/architecture-review-gates.md, gate 6)." >&2
  exit 1
fi
exit 0
