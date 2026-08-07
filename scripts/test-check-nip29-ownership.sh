#!/usr/bin/env bash
# The four falsifiers of #1178, executable.
#
# `scripts/check-nip29-ownership.sh` used to walk `crates/ Packages/ skills/
# tools/`. `Packages/` holds gitignored uniffi output, so a stale binding from
# before a rename made the gate report a TOMBSTONE VIOLATION -- which reads as
# "someone resurrected a deleted spelling" -- against text present in no
# tracked file and in no commit. It cannot reproduce in CI, where the checkout
# is clean, so the cost was paid entirely in local debugging.
#
# Part A falsifies the enumeration itself (`scripts/lib/tracked-corpus.sh`)
# against a purpose-built repository, because the awkward cases -- a path with
# a space, a tracked symlink, a submodule gitlink, a file tracked but deleted
# from the working tree, a single-file batch, invocation from a subdirectory --
# do not occur in this repository today and so cannot be proved against it.
#
# Part B falsifies the GATE, against the real tree, on the issue's own four:
#
#   1. banned spellings in the gitignored generated bindings, every tracked
#      file clean                                        -> the gate passes
#   2. the same spellings in a TRACKED file               -> the gate fails,
#      naming it, once per scanned root and once per scan
#   3. the generated bindings deleted outright            -> the verdict is
#      identical to a tree where they are present and clean
#   4. every ownership and tombstone assertion still fires on its real subject
#
# (2) is the one that matters: a gate made quiet by narrowing its corpus until
# it sees nothing is strictly worse than the flaky version it replaced.
#
# Part B mutates the real working tree, so every managed path is copied aside
# first and put back by a trap. Nothing here runs `git checkout`, stages
# anything, or writes to the index.
set -euo pipefail

SCRIPT_PATH=${BASH_SOURCE[0]}
SCRIPT_DIR=${SCRIPT_PATH%/*}
[[ $SCRIPT_DIR != "$SCRIPT_PATH" ]] || SCRIPT_DIR=.
source "$SCRIPT_DIR/lib/require-commands.sh" || exit 2
require_commands awk cp git grep ln mkdir mktemp mv printf rm || exit 2

ROOT=$(cd "$SCRIPT_DIR/.." && pwd)
GATE=$ROOT/scripts/check-nip29-ownership.sh

TEMP=$(mktemp -d)
RESTORE=$TEMP/restore.sh
: >"$RESTORE"
trap 'bash "$RESTORE" >/dev/null 2>&1 || true; rm -rf "$TEMP"' EXIT INT TERM

CHECKS=0
fail() {
  echo "nip29-ownership test: $*" >&2
  exit 1
}
pass() { CHECKS=$((CHECKS + 1)); }

# ---------------------------------------------------------------------------
# Part A -- the enumeration, against a repository built to hold the awkward
# cases this one does not have.
# ---------------------------------------------------------------------------

source "$SCRIPT_DIR/lib/tracked-corpus.sh" || exit 2

REPO=$TEMP/corpus-repo
mkdir -p "$REPO/src"
git init -q "$REPO"
git -C "$REPO" config user.email "test@example.invalid"
git -C "$REPO" config user.name "corpus test"

printf 'nothing to see\n' >"$REPO/src/plain.rs"
printf 'holds BANNED_TOKEN here\n' >"$REPO/src/with space.rs"
printf 'first\nholds BANNED_TOKEN here\n' >"$REPO/src/two"$'\n'"lines.rs"
printf 'holds BANNED_TOKEN here\n' >"$REPO/src/deleted.rs"
# A symlink to a file that DOES match: excluding symlinks must not lose the
# hit, because the target is scanned under its own name.
ln -s "with space.rs" "$REPO/src/alias.rs"
# A symlink to nothing: grep reading through this is an error, not a hit.
ln -s "nowhere-at-all" "$REPO/src/dangling.rs"
printf 'generated/\n' >"$REPO/.gitignore"
mkdir -p "$REPO/generated"
printf 'holds BANNED_TOKEN here\n' >"$REPO/generated/binding.kt"

git -C "$REPO" add -A
git -C "$REPO" commit -qm "corpus fixture"
printf 'holds BANNED_TOKEN here\n' >"$REPO/src/untracked.rs"
# A submodule gitlink, added to the index directly so the fixture needs no
# network and no second repository on disk.
git -C "$REPO" update-index --add \
  --cacheinfo "160000,$(git -C "$REPO" rev-parse HEAD),src/submodule"
# Tracked, then deleted from the working tree WITHOUT `git rm`: a clean
# checkout still has it.
rm "$REPO/src/deleted.rs"

enumerate() {
  local status=0
  tracked_paths "$@" >/dev/null 2>"$TEMP/enumerate.err" || status=$?
  ENUMERATE_STATUS=$status
  ENUMERATE_ERROR=$(cat "$TEMP/enumerate.err")
}

enumerate "$REPO" src
((ENUMERATE_STATUS == 0)) || fail "the fixture corpus must enumerate: $ENUMERATE_ERROR"
CORPUS=("${TRACKED_PATHS[@]}")

listed() {
  local candidate
  for candidate in "${CORPUS[@]}"; do
    if [[ $candidate == "$1" ]]; then return 0; fi
  done
  return 1
}

listed 'src/with space.rs' || fail "a tracked path holding a space was dropped"
pass
listed "src/two"$'\n'"lines.rs" || fail "a tracked path holding a newline was dropped"
pass
listed 'src/deleted.rs' || fail "a tracked path absent from the working tree was dropped"
pass
! listed 'src/alias.rs' || fail "a tracked symlink was enumerated as a source file"
pass
! listed 'src/dangling.rs' || fail "a dangling tracked symlink was enumerated as a source file"
pass
! listed 'src/submodule' || fail "a submodule gitlink was enumerated as a source file"
pass
! listed 'generated/binding.kt' || fail "a gitignored generated file was enumerated"
pass
! listed 'src/untracked.rs' || fail "an untracked file was enumerated"
pass

SCAN=$(census "$REPO" 'BANNED_TOKEN' "${CORPUS[@]}")

grep -Fq 'src/with space.rs:1:holds BANNED_TOKEN here' <<<"$SCAN" ||
  fail "the scan lost the hit in a path holding a space: $SCAN"
pass
grep -Fq 'lines.rs:2:holds BANNED_TOKEN here' <<<"$SCAN" ||
  fail "the scan lost the hit in a path holding a newline: $SCAN"
pass
# The clean-checkout content, read from the index and reported under the real
# path rather than under a stream name.
grep -Fq 'src/deleted.rs:1:holds BANNED_TOKEN here' <<<"$SCAN" ||
  fail "the scan lost a tracked file that is absent from the working tree: $SCAN"
pass
! grep -Fq 'generated/binding.kt' <<<"$SCAN" ||
  fail "the scan read a gitignored generated file: $SCAN"
pass
! grep -Fq 'src/untracked.rs' <<<"$SCAN" ||
  fail "the scan read an untracked file: $SCAN"
pass
! grep -Fq 'src/alias.rs' <<<"$SCAN" ||
  fail "the scan read through a tracked symlink: $SCAN"
pass
! grep -Fq 'grep exited' <<<"$SCAN" ||
  fail "the scan errored rather than completing: $SCAN"
pass

# One file is the batch size that makes grep drop the filename unless `-H` is
# given, which would report a bare `1:...` naming nothing.
SINGLE=$(census "$REPO" 'BANNED_TOKEN' 'src/with space.rs')
[[ $SINGLE == 'src/with space.rs:1:holds BANNED_TOKEN here' ]] ||
  fail "a single-file scan did not name the file it matched: $SINGLE"
pass

# A pathspec that matches nothing is refused rather than scanned as air.
enumerate "$REPO" docs
((ENUMERATE_STATUS != 0)) || fail "an empty pathspec must be refused, not scanned"
grep -Fq 'would be vacuous' <<<"$ENUMERATE_ERROR" ||
  fail "an empty pathspec was refused for the wrong reason: $ENUMERATE_ERROR"
pass

# A pathspec resolved from a subdirectory and a path reported against the top
# level would disagree, so the root argument must BE the top level.
enumerate "$REPO/src" .
((ENUMERATE_STATUS != 0)) || fail "a subdirectory root must be refused"
grep -Fq 'not the repository top level' <<<"$ENUMERATE_ERROR" ||
  fail "a subdirectory root was refused for the wrong reason: $ENUMERATE_ERROR"
pass

mkdir -p "$TEMP/not-a-repo"
enumerate "$TEMP/not-a-repo" .
((ENUMERATE_STATUS != 0)) || fail "a directory outside any repository must be refused"
grep -Fq 'not inside a git repository' <<<"$ENUMERATE_ERROR" ||
  fail "a non-repository root was refused for the wrong reason: $ENUMERATE_ERROR"
pass

# ---------------------------------------------------------------------------
# Part B -- the gate, against the real tree.
# ---------------------------------------------------------------------------

MANAGED=0

tree_state() {
  (cd "$ROOT" && git status --porcelain --untracked-files=all -- crates Packages skills tools)
}
TREE_BEFORE=$(tree_state)

# Copy a path aside and record how to put it back exactly as it is now, so a
# mutation below can never survive this script.
manage() {
  local relative=$1 kept
  MANAGED=$((MANAGED + 1))
  kept=$TEMP/kept/$MANAGED
  mkdir -p "$TEMP/kept"
  if [[ -e $ROOT/$relative ]]; then
    cp -R "$ROOT/$relative" "$kept"
    printf 'rm -rf %q\ncp -R %q %q\n' "$ROOT/$relative" "$kept" "$ROOT/$relative" >>"$RESTORE"
  else
    printf 'rm -rf %q\n' "$ROOT/$relative" >>"$RESTORE"
  fi
}

restore_all() { bash "$RESTORE" >/dev/null 2>&1 || true; }

run_gate() {
  GATE_STATUS=0
  GATE_OUTPUT=$(cd "$ROOT" && bash "$GATE" 2>&1) || GATE_STATUS=$?
}

expect_green() {
  local label=$1
  run_gate
  ((GATE_STATUS == 0)) || fail "$label: the gate went red -- $GATE_OUTPUT"
  pass
}

# A red must be the RIGHT red: the verdict line, and the path that caused it.
expect_red() {
  local label=$1 verdict=$2 named=$3
  run_gate
  ((GATE_STATUS != 0)) || fail "$label: the gate stayed green"
  grep -Fq -- "$verdict" <<<"$GATE_OUTPUT" ||
    fail "$label: red for the wrong reason -- $GATE_OUTPUT"
  [[ -z $named ]] || grep -Fq -- "$named" <<<"$GATE_OUTPUT" ||
    fail "$label: the red did not name $named -- $GATE_OUTPUT"
  pass
}

# Appended as a whole-line comment, so the mutation is inert in every language
# involved and the file it lands in still compiles while the gate reads it.
taint() {
  local relative=$1 text=$2
  manage "$relative"
  printf '\n// %s\n' "$text" >>"$ROOT/$relative"
}

GENERATED_SWIFT=Packages/NMP/Sources/NMPFFI/nmp_ffi.swift
GENERATED_KOTLIN=Packages/NMPKotlin/src/main/kotlin/uniffi/nmp_ffi/nmp_ffi.kt

# The whole banned vocabulary of every directory-wide scan the gate runs, so
# falsifier 1 covers all of them rather than the first one.
BANNED_SWIFT='func groupDiscoveryDemand() {}
let p = GroupPredicate.anyOf
let m = memberIs(x)
func composeChatReply() {}
struct FfiGroupReplyParent {}'
BANNED_KOTLIN='fun groupDiscoveryDemand() {}
val p = GroupPredicate.anyOf
val m = memberIs(x)
fun composeChatReply() {}
class FfiGroupReplyParent'
CLEAN_BINDING='// generated by uniffi -- fixture, not real output
func nothingBannedHere() {}'

write_generated() {
  local swift=$1 kotlin=$2
  mkdir -p "$ROOT/${GENERATED_SWIFT%/*}" "$ROOT/${GENERATED_KOTLIN%/*}"
  printf '%s\n' "$swift" >"$ROOT/$GENERATED_SWIFT"
  printf '%s\n' "$kotlin" >"$ROOT/$GENERATED_KOTLIN"
}

# The generated trees are gitignored, so a developer running this may or may
# not have them. Both are managed, whichever state they are in.
manage Packages/NMP/Sources/NMPFFI
manage Packages/NMPKotlin/src/main/kotlin/uniffi

# The tree as found must pass, or every red below proves nothing.
run_gate
((GATE_STATUS == 0)) ||
  fail "the unmutated tree must pass before anything is mutated -- $GATE_OUTPUT"
pass

# ---- falsifier 3, first half: bindings present and clean -----------------
write_generated "$CLEAN_BINDING" "$CLEAN_BINDING"
run_gate
((GATE_STATUS == 0)) || fail "clean generated bindings must not fail the gate -- $GATE_OUTPUT"
VERDICT_WITH_BINDINGS=$GATE_OUTPUT
pass

# ---- falsifier 1: banned spellings in the generated bindings only --------
write_generated "$BANNED_SWIFT" "$BANNED_KOTLIN"
run_gate
((GATE_STATUS == 0)) ||
  fail "a stale generated binding still decides the verdict -- $GATE_OUTPUT"
[[ $GATE_OUTPUT == "$VERDICT_WITH_BINDINGS" ]] ||
  fail "a stale generated binding changed what the gate SAID, without changing
       whether it passed:
       clean bindings: $VERDICT_WITH_BINDINGS
       stale ones:     $GATE_OUTPUT"
pass

# ---- falsifier 3, second half: bindings absent ---------------------------
rm -rf "$ROOT/Packages/NMP/Sources/NMPFFI" \
  "$ROOT/Packages/NMPKotlin/src/main/kotlin/uniffi"
run_gate
((GATE_STATUS == 0)) || fail "deleting the generated bindings failed the gate -- $GATE_OUTPUT"
[[ $GATE_OUTPUT == "$VERDICT_WITH_BINDINGS" ]] ||
  fail "the verdict changed when the generated bindings were deleted:
       with bindings: $VERDICT_WITH_BINDINGS
       without:       $GATE_OUTPUT"
pass

restore_all

# ---- falsifier 2: the same spellings in a TRACKED file -------------------
#
# Once per scanned root, so narrowing the corpus cannot have quietly dropped
# one of the four, and once per directory-wide scan, so it cannot have dropped
# one of the five.

taint crates/nmp-nipc7/src/lib.rs 'group_discovery_demand'
expect_red "tracked crates/ file" \
  "a deleted NIP-29 publication/discovery spelling reappeared" \
  "crates/nmp-nipc7/src/lib.rs"
restore_all

taint Packages/NMP/Sources/NMP/NIP29.swift 'group_discovery_demand'
expect_red "tracked Packages/ file" \
  "a deleted NIP-29 publication/discovery spelling reappeared" \
  "Packages/NMP/Sources/NMP/NIP29.swift"
restore_all

taint skills/nmp-dev/SKILL.md 'group_discovery_demand'
expect_red "tracked skills/ file" \
  "a deleted NIP-29 publication/discovery spelling reappeared" \
  "skills/nmp-dev/SKILL.md"
restore_all

taint tools/nip29-consumer/src/main.rs 'group_discovery_demand'
expect_red "tracked tools/ file" \
  "a deleted NIP-29 publication/discovery spelling reappeared" \
  "tools/nip29-consumer/src/main.rs"
restore_all

taint crates/nmp-nipc7/src/lib.rs 'GroupPredicate::AnyOf'
expect_red "predicate tombstone" \
  "a deleted NIP-29 predicate leaf or predicate-level combinator reappeared" \
  "crates/nmp-nipc7/src/lib.rs"
restore_all

taint crates/nmp-nipc7/src/lib.rs 'member_is(pubkey)'
expect_red "overclaiming membership spelling" \
  "an overclaiming exact-membership/admin spelling reappeared" \
  "crates/nmp-nipc7/src/lib.rs"
restore_all

taint crates/nmp-nipc7/src/lib.rs 'compose_chat_reply'
expect_red "retired C7 q-reply composer" \
  "the retired C7 q-reply composer or its falsifier reappeared" \
  "crates/nmp-nipc7/src/lib.rs"
restore_all

taint Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/NIP29.kt 'FfiGroupReplyParent'
expect_red "superseded native surface" \
  "superseded NIP-29 native surface reappeared" \
  "Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/NIP29.kt"
restore_all

taint crates/nmp/src/lib.rs 'pinned-host-hex'
expect_red "removed routing/journal spelling" \
  "a removed routing spelling reappeared" \
  "crates/nmp/src/lib.rs"
restore_all

# A tracked file the developer deleted without `git rm` is still judged, on the
# content a clean checkout would have, and the scan does not error on it.
manage crates/nmp-nipc7/src/lib.rs
rm "$ROOT/crates/nmp-nipc7/src/lib.rs"
run_gate
((GATE_STATUS != 0)) || fail "a deleted required path must fail the gate"
grep -Fq 'required path is missing' <<<"$GATE_OUTPUT" ||
  fail "a deleted required path failed for the wrong reason -- $GATE_OUTPUT"
pass
restore_all

manage skills/nmp-dev/references/testing/INDEX.md
rm "$ROOT/skills/nmp-dev/references/testing/INDEX.md"
expect_green "a tracked file deleted from the working tree"
restore_all

# ---- falsifier 4: every other assertion still fires on its real subject ---

manage crates/nmp/src/nip29/predicate.rs
mv "$ROOT/crates/nmp/src/nip29/predicate.rs" "$TEMP/predicate.rs.aside"
expect_red "a missing required path" "required path is missing" \
  "crates/nmp/src/nip29/predicate.rs"
restore_all

manage crates/nmp/src/group.rs
printf '// a pre-#1033 single-host door\n' >"$ROOT/crates/nmp/src/group.rs"
expect_red "a resurrected pre-#1033 path" "a pre-#1033 single-host path reappeared" \
  "crates/nmp/src/group.rs"
restore_all

manage crates/nmp-nip29/src/discovery.rs
printf '\npub fn leaked() -> WriteIntent { unimplemented!() }\n' \
  >>"$ROOT/crates/nmp-nip29/src/discovery.rs"
expect_red "WriteIntent below the facade" \
  "the engine-free NIP-29 crate referenced WriteIntent" \
  "crates/nmp-nip29/src/discovery.rs"
restore_all

# The negative control `crates/nmp-bdd/src/world/group_surface.rs` relies on:
# an UNTRACKED probe dropped into the engine-free crate, which the two
# per-directory `*.rs` loops still read. Narrowing those loops to the tracked
# corpus would silently retire that scenario's only evidence.
manage crates/nmp-nip29/src/kind_branch_probe.rs
printf 'pub fn privileges_chat(kind: nostr::Kind) -> bool {\n    kind == nostr::Kind::from(9)\n}\n' \
  >"$ROOT/crates/nmp-nip29/src/kind_branch_probe.rs"
expect_red "the untracked kind-branch probe" \
  "NIP-29 re-acquired chat/content-schema ownership it does not have" \
  "crates/nmp-nip29/src/kind_branch_probe.rs"
restore_all

manage crates/nmp/src/nip29/predicate.rs
printf '\npub struct Placeholder;\n' >"$ROOT/crates/nmp/src/nip29/predicate.rs"
expect_red "a gutted predicate module" "the literal group-id predicate leaf is missing" ""
restore_all

manage crates/nmp/src/nip29/group.rs
printf '\npub fn publish_composed() {}\n' >>"$ROOT/crates/nmp/src/nip29/group.rs"
expect_red "a second group write lifecycle" "a second write lifecycle for groups appeared" ""
restore_all

manage crates/nmp/src/nip29/read.rs
printf '\npub fn stream() {}\n' >>"$ROOT/crates/nmp/src/nip29/read.rs"
expect_red "a group-shaped stream lifecycle" \
  "a group-shaped subscribe/stream lifecycle appeared beside the one observe door" ""
restore_all

# An UNTRACKED scratch file under a scanned root is text in no commit, which is
# exactly what made this gate untrustworthy. It must not decide the verdict.
manage crates/nmp/src/scratch-note.txt
printf 'pinned-host-hex group_discovery_demand member_is(x)\n' \
  >"$ROOT/crates/nmp/src/scratch-note.txt"
expect_green "an untracked scratch file under crates/"
restore_all

# The enumeration cannot silently return nothing: with git unable to answer,
# the gate refuses rather than scanning air and reporting a clean tree.
GATE_STATUS=0
GATE_OUTPUT=$(cd "$ROOT" && GIT_DIR=/nonexistent-for-this-check bash "$GATE" 2>&1) ||
  GATE_STATUS=$?
((GATE_STATUS != 0)) || fail "an unanswerable enumeration passed the gate -- $GATE_OUTPUT"
grep -Fq 'tracked-corpus:' <<<"$GATE_OUTPUT" ||
  fail "an unanswerable enumeration failed for the wrong reason -- $GATE_OUTPUT"
pass

restore_all

# The tree must be exactly as it was found.
TREE_AFTER=$(tree_state)
[[ $TREE_AFTER == "$TREE_BEFORE" ]] || fail "the tree was left mutated:
$TREE_AFTER"

run_gate
((GATE_STATUS == 0)) || fail "the gate must be green again on the restored tree -- $GATE_OUTPUT"

printf 'nip29-ownership test: %s checks passed (enumeration and gate)\n' "$CHECKS"
