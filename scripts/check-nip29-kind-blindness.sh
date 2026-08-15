#!/usr/bin/env bash
# #1124 (PROTOCOL-KINDBLINDNESS-001..005): structural evidence that the
# NIP-29 group publication/read path is kind-blind -- no kind it privileges,
# no kind it rejects, no branch anywhere that reads the kind -- and that the
# crate's own kind catalogue is bounded to exactly the kinds NIP-29 itself
# defines (#989: 9000-9022 moderation, 39000-39002 discovery).
#
# This is the REPLACEMENT for the weak form: the retired approach proved
# "no kind branch" by writing a decoy source file at runtime
# (`kind_branch_probe.rs`, `crates/nmp-bdd/src/world/group_surface.rs`) and
# grepping for the single literal `Kind::from(9)`/`= 9;` -- a blacklist of
# one value that a differently-numbered foreign kind (or a branch that reads
# `.kind` without comparing it to a literal at all) would have sailed past.
#
# What replaces it enumerates instead of blacklisting, and fails closed on
# anything it cannot classify -- the same shape as `check-dependency-
# direction.py` (#922): every `Kind::from(<n>)` call and every `: u16 = <n>;`
# binding in the two files NIP-29 legitimately owns kinds in
# (`operations.rs`, `discovery.rs`) must resolve to a value in the exact
# owned set; anything else is a structural violation, not a matched pattern.
# Separately, `Kind`/`.kind` may not appear AT ALL in the crate's other
# files (`context.rs`, `lib.rs`, or a new file such as the retired probe) --
# this is what makes an app-defined kind branch, wherever it is added,
# structurally unrepresentable rather than merely unmatched by a regex.
#
# Self-test with a real mutated fixture, run from the SAME functions this
# script exposes: scripts/test-nip29-kind-blindness.sh.
#
# #1653 absorbed `scripts/check-nip29-ownership.sh`'s own kind-9/chat-schema
# decoy-name ban here (its former `:116-126`). That version truncated its scan
# at the file's FIRST `#[cfg(test)]` marker via a plain `awk .../{exit}`,
# reading only the first 313 of `context.rs`'s 896 lines -- a real item placed
# after that marker, in a file that legitimately has test fixtures near its
# top, was invisible to it. `check_no_owned_decoy_names` below reuses this
# script's own brace-depth-aware `non_test_source`, already proven correct by
# `scripts/test-nip29-kind-blindness.sh`'s RED-2/RED-5/GREEN cases, instead of
# a second, weaker text-truncation rule.
#
# #1074 evidence for PROTOCOL-KINDBLINDNESS-001..005
# (features/groups/kind-blindness.feature).
set -euo pipefail

SCRIPT_PATH=${BASH_SOURCE[0]}
SCRIPT_DIR=${SCRIPT_PATH%/*}
[[ $SCRIPT_DIR != "$SCRIPT_PATH" ]] || SCRIPT_DIR=.
source "$SCRIPT_DIR/lib/require-commands.sh" || exit 2
require_commands awk grep sort || exit 2

# The kinds NIP-29 itself defines, and nothing else (#989). Nine moderation
# kinds owned by operations.rs, three discovery kinds owned by discovery.rs.
ALLOWED_KINDS=(9000 9001 9002 9005 9007 9008 9009 9021 9022 39000 39001 39002)

fail() {
  echo "nip29-kind-blindness: $*" >&2
  exit 1
}

is_allowed() {
  local needle=$1 candidate
  for candidate in "${ALLOWED_KINDS[@]}"; do
    [[ $candidate == "$needle" ]] && return 0
  done
  return 1
}

# Non-test source of one file: strips every `#[cfg(test)]`-attributed item
# by brace depth (not merely "everything from the first occurrence
# onward" -- a #[cfg(test)] block is not guaranteed to be the last item in
# the file, and truncating at the first marker would silently stop
# inspecting anything appended after it). Fixture kinds inside `mod tests`
# (kind 9, kind 20, kind 30023, ...) are excluded; a real item placed
# anywhere else, before OR after a test module, is still inspected.
non_test_source() {
  awk '
    BEGIN { skip_from = -1; depth = 0 }
    {
      line = $0
      if (skip_from == -1 && line ~ /^#\[cfg\(test\)\]/) {
        skip_from = depth
        next
      }
      opens = gsub(/\{/, "{", line)
      closes = gsub(/\}/, "}", line)
      if (skip_from != -1) {
        depth += opens
        depth -= closes
        if (depth <= skip_from) {
          skip_from = -1
        }
        next
      }
      depth += opens
      depth -= closes
      print line
    }
  ' "$1"
}

# Check A: every numeric kind literal the crate's two kind-OWNING files
# (operations.rs, discovery.rs) mention must be in ALLOWED_KINDS. Enumerated,
# not blacklisted: a foreign kind at ANY value fails, not only kind 9.
check_owned_kind_literals() {
  local src_dir=$1 file literal found_foreign=0
  for file in "$src_dir/operations.rs" "$src_dir/discovery.rs"; do
    # #1653 (hole 3): a renamed or moved owning file must fail closed, not
    # be silently skipped. A bare `continue` here let `mv discovery.rs
    # disc.rs` plus a foreign kind constant in the renamed file pass
    # undetected -- this loop is the ONLY place that enumerates kind
    # literals against the allow-list, so its silent absence was a hole,
    # not a graceful degradation.
    [[ -f $file ]] || fail "expected NIP-29 kind-owning source is missing: $file"
    while IFS= read -r literal; do
      [[ -n $literal ]] || continue
      if ! is_allowed "$literal"; then
        echo "$file: kind literal $literal is not in NIP-29's own kind set (${ALLOWED_KINDS[*]})" >&2
        found_foreign=1
      fi
    done < <(
      non_test_source "$file" |
        grep -oE 'Kind::from\([0-9]+|:[[:space:]]*u16[[:space:]]*=[[:space:]]*[0-9]+' |
        grep -oE '[0-9]+$'
    )
  done
  ((found_foreign == 0)) || fail "the NIP-29-owned kind catalogue grew a kind NIP-29 does not define"
}

# Check B: no OTHER file in the crate may reference `Kind` or `.kind` at
# all -- not with a matched literal, not with a variable, not with a
# comparison. This is what makes "read the kind and branch on it" structurally
# absent rather than merely untriggered: the retired probe technique (drop a
# new .rs file into this directory with a function that takes `kind:
# nostr::Kind` and compares it) is caught here because the new file is
# globbed and is not operations.rs, regardless of which kind literal (or no
# literal at all -- an opaque comparison would still be caught) it uses.
check_no_kind_reads_outside_ownership() {
  local src_dir=$1 file found=0
  for file in "$src_dir"/*.rs; do
    [[ -f $file ]] || continue
    case "$(basename "$file")" in
    operations.rs) continue ;; # the one file NIP-29 mints its own kinds in
    esac
    local hit
    hit=$(non_test_source "$file" | grep -nE '\bKind\b|\.kind\b' || true)
    if [[ -n $hit ]]; then
      printf '%s:\n%s\n' "$file" "$hit" >&2
      found=1
    fi
  done
  ((found == 0)) || fail "a kind-blind file references Kind or .kind -- a kind branch or a new kind-reading surface appeared"
}

# Check C (#1653, absorbed from the former check-nip29-ownership.sh:116-126):
# no file in the crate may re-acquire kind:9/chat-schema ownership by NAME,
# even a name that never resolves to a numeric literal Check A or B would
# catch -- a constant called CHAT_KIND, a `compose_chat`/`GroupReply`
# function, or the kind-30315-adjacent status-row pairing NIP-C7 owns.
# `non_test_source` (not a truncating scan) is what makes this correct on a
# file with fixtures ABOVE its own logic, not only below it.
check_no_owned_decoy_names() {
  local src_dir=$1 file found=0
  for file in "$src_dir"/*.rs; do
    [[ -f $file ]] || continue
    local hit
    hit=$(non_test_source "$file" |
      grep -nE 'CHAT_KIND|Kind::from\(9\)|=[[:space:]]*9;|compose_chat|GroupReply|recipient_pubkeys|group_content_demand|\[9[^0-9]+30315\]' || true)
    if [[ -n $hit ]]; then
      printf '%s:\n%s\n' "$file" "$hit" >&2
      found=1
    fi
  done
  ((found == 0)) || fail "NIP-29 re-acquired chat/content-schema ownership it does not have"
}

SRC_DIR=${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/crates/nmp-nip29/src}
[[ -d $SRC_DIR ]] || fail "no such NIP-29 source directory: $SRC_DIR"

check_owned_kind_literals "$SRC_DIR"
check_no_kind_reads_outside_ownership "$SRC_DIR"
check_no_owned_decoy_names "$SRC_DIR"

echo "nip29-kind-blindness: ok"
