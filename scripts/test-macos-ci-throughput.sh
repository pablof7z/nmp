#!/usr/bin/env bash
# Mutation falsifiers for scripts/check-macos-ci-throughput.sh.

set -euo pipefail

ROOT=$(git rev-parse --show-toplevel)
CHECKER="$ROOT/scripts/check-macos-ci-throughput.sh"
TEMP_ROOT=$(mktemp -d)
FIXTURE_ROOT="$TEMP_ROOT/repo"
trap 'rm -r "$TEMP_ROOT"' EXIT

fail() {
  echo "macOS CI throughput test: $*" >&2
  exit 1
}

reset_fixture() {
  rm -rf "$FIXTURE_ROOT"
  mkdir -p \
    "$FIXTURE_ROOT/Packages/NMP/Tests/NMPTests" \
    "$FIXTURE_ROOT/apps/Falsifier"
  cp -R "$ROOT/.github" "$FIXTURE_ROOT/.github"
  cp \
    "$ROOT/Packages/NMP/Tests/NMPTests/BoundedRelayTimeSharingTests.swift" \
    "$ROOT/Packages/NMP/Tests/NMPTests/ControlledRelayHarness.swift" \
    "$ROOT/Packages/NMP/Tests/NMPTests/RelayInformationTests.swift" \
    "$FIXTURE_ROOT/Packages/NMP/Tests/NMPTests/"
  cp "$ROOT/apps/Falsifier/project.yml" "$FIXTURE_ROOT/apps/Falsifier/project.yml"
}

expect_failure() {
  local label=$1
  local expected=$2
  local output
  if output=$(bash "$CHECKER" "$FIXTURE_ROOT" 2>&1); then
    fail "$label mutation unexpectedly passed"
  fi
  grep -Fq -- "$expected" <<< "$output" ||
    fail "$label mutation failed for the wrong reason: $output"
}

group_for() {
  local workflow=$1
  local event=$2
  local pr_or_ref=$3
  printf '%s-%s-%s\n' "$workflow" "$event" "$pr_or_ref"
}

bash "$CHECKER" "$ROOT"

# The checked expression and this direct model agree on the required collision
# and isolation cases.
same_pr_a=$(group_for "macOS qualification" pull_request 1050)
same_pr_b=$(group_for "macOS qualification" pull_request 1050)
[[ "$same_pr_a" == "$same_pr_b" ]] || fail "same-PR updates do not collide"
[[ "$same_pr_a" != "$(group_for "macOS qualification" pull_request 1051)" ]] ||
  fail "unrelated PRs collide"
same_ref_a=$(group_for "macOS qualification" push refs/heads/master)
same_ref_b=$(group_for "macOS qualification" push refs/heads/master)
[[ "$same_ref_a" == "$same_ref_b" ]] || fail "same-ref updates do not collide"
[[ "$same_pr_a" != "$same_ref_a" ]] ||
  fail "PR and master push collide"
[[ "$same_ref_a" != \
   "$(group_for "macOS qualification" push refs/heads/release)" ]] ||
  fail "different refs collide"
[[ "$same_pr_a" != "$(group_for "Other workflow" pull_request 1050)" ]] ||
  fail "different workflows collide"

reset_fixture
cat >> "$FIXTURE_ROOT/.github/workflows/macos-qualification.yml" <<'YAML'
  reintroduced-second-macos-owner:
    runs-on: macos-14
    steps:
      - run: true
YAML
expect_failure "second macOS job" "expected exactly one PR macOS job, found 2"

reset_fixture
sed -i.bak \
  's#      - name: Test the Swift package#      - name: Removed Swift suite#' \
  "$FIXTURE_ROOT/.github/workflows/macos-qualification.yml"
rm "$FIXTURE_ROOT/.github/workflows/macos-qualification.yml.bak"
expect_failure "removed suite marker" "      - name: Test the Swift package"

reset_fixture
sed -i.bak \
  's/github\.event\.pull_request\.number || github\.ref/github.sha/' \
  "$FIXTURE_ROOT/.github/workflows/macos-qualification.yml"
rm "$FIXTURE_ROOT/.github/workflows/macos-qualification.yml.bak"
expect_failure "per-head key" "github.event.pull_request.number || github.ref"

reset_fixture
sed -i.bak \
  's/github\.workflow/github.repository/' \
  "$FIXTURE_ROOT/.github/workflows/macos-qualification.yml"
rm "$FIXTURE_ROOT/.github/workflows/macos-qualification.yml.bak"
expect_failure "cross-workflow group" 'group: ${{ github.workflow }}'

reset_fixture
sed -i.bak \
  's/cancel-in-progress: true/cancel-in-progress: false/' \
  "$FIXTURE_ROOT/.github/workflows/macos-qualification.yml"
rm "$FIXTURE_ROOT/.github/workflows/macos-qualification.yml.bak"
expect_failure "disabled cancellation" "cancel-in-progress: true"

reset_fixture
sed -i.bak \
  's/  pull_request:/  disabled_pull_request:/' \
  "$FIXTURE_ROOT/.github/workflows/macos-qualification.yml"
rm "$FIXTURE_ROOT/.github/workflows/macos-qualification.yml.bak"
expect_failure "removed PR trigger" "pull_request:"

reset_fixture
sed -i.bak \
  's/mode=--macos-only/mode=--sim-only/' \
  "$FIXTURE_ROOT/.github/workflows/macos-qualification.yml"
rm "$FIXTURE_ROOT/.github/workflows/macos-qualification.yml.bak"
expect_failure "thick PR scope" 'echo "mode=--macos-only"'

reset_fixture
sed -i.bak \
  's/echo "mode=" >> "\$GITHUB_OUTPUT"/echo "mode=--macos-only" >> "$GITHUB_OUTPUT"/' \
  "$FIXTURE_ROOT/.github/workflows/macos-qualification.yml"
rm "$FIXTURE_ROOT/.github/workflows/macos-qualification.yml.bak"
expect_failure "removed full master packaging" 'echo "mode=" >> "$GITHUB_OUTPUT"'

# #1240: the builder installs the Apple targets it builds for. A workflow-side
# target list is a second owner of that fact and hides a builder that stopped.
reset_fixture
sed -i.bak \
  '/scripts\/build-swift-xcframework.sh/i\
          rustup target add aarch64-apple-darwin' \
  "$FIXTURE_ROOT/.github/workflows/macos-qualification.yml"
rm "$FIXTURE_ROOT/.github/workflows/macos-qualification.yml.bak"
expect_failure "workflow-owned target install" "rustup target add"

reset_fixture
sed -i.bak \
  '/scripts\/build-swift-xcframework.sh/a\
          scripts/build-swift-xcframework.sh' \
  "$FIXTURE_ROOT/.github/workflows/macos-qualification.yml"
rm "$FIXTURE_ROOT/.github/workflows/macos-qualification.yml.bak"
expect_failure "duplicated native build" "expected exactly one Apple native build, found 2"

reset_fixture
sed -i.bak \
  's/run: swift build/run: swift build \\&\\& xcodebuild test/' \
  "$FIXTURE_ROOT/.github/workflows/macos-qualification.yml"
rm "$FIXTURE_ROOT/.github/workflows/macos-qualification.yml.bak"
expect_failure "reintroduced simulator orchestration" "xcodebuild test"

reset_fixture
sed -i.bak \
  's/testAutoRoutedWriteProgressesPastAWaitingRelayLaneWithoutClaimingSettlement/removedDurableAutoRoutedProof/' \
  "$FIXTURE_ROOT/Packages/NMP/Tests/NMPTests/BoundedRelayTimeSharingTests.swift"
rm "$FIXTURE_ROOT/Packages/NMP/Tests/NMPTests/BoundedRelayTimeSharingTests.swift.bak"
expect_failure "removed bounded-session Swift proof" "testAutoRoutedWriteProgressesPastAWaitingRelayLaneWithoutClaimingSettlement"

reset_fixture
sed -i.bak \
  '/- name: Install pinned Rust toolchain/a\
      - name: Portable Rust tests drifted onto macOS\
        run: cargo test' \
  "$FIXTURE_ROOT/.github/workflows/macos-qualification.yml"
rm "$FIXTURE_ROOT/.github/workflows/macos-qualification.yml.bak"
expect_failure "portable work drifted onto macOS" "exactly six named Apple qualification steps"

echo "macOS CI throughput test: baseline and thirteen mutations passed"
