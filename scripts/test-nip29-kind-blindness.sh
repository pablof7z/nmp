#!/usr/bin/env bash
# Self-test for scripts/check-nip29-kind-blindness.sh (#1124,
# PROTOCOL-KINDBLINDNESS-005): proves the checker can actually fail, against
# real mutated copies of the real crate source, not a hand-rolled duplicate
# regex (the tautological "positive control" shape rejected in #1168/#1158).
#
# Every RED case here copies the genuine crates/nmp-nip29/src tree to a temp
# directory, mutates the copy, and invokes the SAME production script
# (scripts/check-nip29-kind-blindness.sh) against that copy via its
# documented `[SRC_DIR]` argument -- the identical function
# `check_owned_kind_literals`/`check_no_kind_reads_outside_ownership` that
# runs in CI, not a second implementation of the rule.

set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
CHECKER="$ROOT/scripts/check-nip29-kind-blindness.sh"
REAL_SRC="$ROOT/crates/nmp-nip29/src"
BASH_BIN=$(command -v bash)
TEMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/nmp-nip29-kind-blindness-test.XXXXXX")
trap 'rm -rf "$TEMP_ROOT"' EXIT

fail() {
  echo "nip29-kind-blindness test: $*" >&2
  exit 1
}

fresh_copy() {
  local name=$1
  local dest="$TEMP_ROOT/$name/src"
  mkdir -p "$(dirname "$dest")"
  cp -r "$REAL_SRC" "$dest"
  printf '%s\n' "$dest"
}

expect_pass() {
  local label=$1 src_dir=$2
  local output
  if ! output=$("$BASH_BIN" "$CHECKER" "$src_dir" 2>&1); then
    fail "$label unexpectedly failed: $output"
  fi
  grep -qF 'nip29-kind-blindness: ok' <<<"$output" ||
    fail "$label did not report ok: $output"
}

expect_fail() {
  local label=$1 src_dir=$2 expected=$3
  local output
  if output=$("$BASH_BIN" "$CHECKER" "$src_dir" 2>&1); then
    fail "$label unexpectedly passed"
  fi
  grep -qF -- "$expected" <<<"$output" ||
    fail "$label failed for the wrong reason; missing '$expected': $output"
}

# --- GREEN: the real, unmutated tree passes. -------------------------------
GREEN_SRC=$(fresh_copy green)
expect_pass "the real unmutated tree" "$GREEN_SRC"

# --- RED 1: the retired probe technique -- a new file with a function that
# reads an app-supplied kind and branches on it, exactly the shape
# `crates/nmp-bdd/src/world/group_surface.rs`'s `gate_rejects_a_kind_branch`
# used to drop at `crates/nmp-nip29/src/kind_branch_probe.rs`.
PROBE_SRC=$(fresh_copy probe)
cat >"$PROBE_SRC/kind_branch_probe.rs" <<'EOF'
pub fn privileges_chat(kind: nostr::Kind) -> bool {
    kind == nostr::Kind::from(9)
}
EOF
expect_fail "a dropped-in kind-branch probe file" "$PROBE_SRC" \
  "a kind branch or a new kind-reading surface appeared"

# --- RED 2: a kind branch INSIDE an existing kind-blind file, not a new
# file -- proves the check does not merely notice a new filename. Inserted
# BEFORE the file's `#[cfg(test)]` marker (not appended at the end), so it
# lands in the non-test region the checker actually inspects -- appending
# after that marker would land inside the excluded test module and prove
# nothing.
INLINE_SRC=$(fresh_copy inline)
python3 - "$INLINE_SRC/context.rs" <<'PY'
import sys

path = sys.argv[1]
with open(path) as handle:
    content = handle.read()
marker = "#[cfg(test)]"
index = content.index(marker)
injected = (
    "pub fn privileges_c7(builder: &nmp_grammar::EventBuilder) -> bool {\n"
    "    builder.kind == nostr::Kind::from(9)\n"
    "}\n\n"
)
content = content[:index] + injected + content[index:]
with open(path, "w") as handle:
    handle.write(content)
PY
expect_fail "a kind branch inserted before context.rs's test module" "$INLINE_SRC" \
  "a kind branch or a new kind-reading surface appeared"

# --- RED 3: a foreign kind literal smuggled into the OWNING files at a
# value other than the one legacy literal (9) the retired grep blacklisted --
# proves the replacement enumerates and diffs an allow-list rather than
# matching one remembered value.
FOREIGN_SRC=$(fresh_copy foreign)
sed -i.bak 's/const CREATE_INVITE: u16 = 9009;/const CREATE_INVITE: u16 = 9009;\nconst SNEAKY_CHAT_KIND: u16 = 1;/' \
  "$FOREIGN_SRC/operations.rs"
rm -f "$FOREIGN_SRC/operations.rs.bak"
expect_fail "a foreign kind literal (1, never blacklisted before) in operations.rs" "$FOREIGN_SRC" \
  "kind literal 1 is not in NIP-29's own kind set"

# --- RED 4: a foreign kind literal in discovery.rs, the other owning file.
DISCOVERY_SRC=$(fresh_copy discovery)
sed -i.bak 's/pub const GROUP_MEMBERS_KIND: u16 = 39002;/pub const GROUP_MEMBERS_KIND: u16 = 39002;\npub const SNEAKY_KIND: u16 = 12345;/' \
  "$DISCOVERY_SRC/discovery.rs"
rm -f "$DISCOVERY_SRC/discovery.rs.bak"
expect_fail "a foreign kind literal (12345) in discovery.rs" "$DISCOVERY_SRC" \
  "kind literal 12345 is not in NIP-29's own kind set"

# --- GREEN AGAIN: a fixture-only kind reference inside #[cfg(test)] must
# NOT trip the check -- proves the cfg(test) exclusion works on a REAL
# mutation, not only on the code that was already there.
TEST_ONLY_SRC=$(fresh_copy test_only)
cat >>"$TEST_ONLY_SRC/context.rs" <<'EOF'

#[cfg(test)]
mod extra_fixture_tests {
    #[test]
    fn a_fixture_may_reference_any_kind_literal_it_likes() {
        let _ = nostr::Kind::from(1u16);
    }
}
EOF
expect_pass "a fixture-only kind literal inside cfg(test)" "$TEST_ONLY_SRC"

# --- RED 5: a kind branch appended AFTER the file's existing #[cfg(test)]
# module -- proves the cfg(test) exclusion is brace-depth-scoped rather than
# "stop looking after the first #[cfg(test)] marker", which would have let
# a real item placed after the test module through undetected.
AFTER_TESTS_SRC=$(fresh_copy after_tests)
cat >>"$AFTER_TESTS_SRC/context.rs" <<'EOF'

pub fn privileges_c7_after_tests(builder: &nmp_grammar::EventBuilder) -> bool {
    builder.kind == nostr::Kind::from(9)
}
EOF
expect_fail "a kind branch appended after context.rs's test module" "$AFTER_TESTS_SRC" \
  "a kind branch or a new kind-reading surface appeared"

echo "nip29-kind-blindness test: ok (5 red, 2 green, all against real mutated fixtures)"
