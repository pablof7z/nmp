#!/usr/bin/env bash
# Structural contract for issue #1058. actionlint owns generic workflow syntax;
# this checker owns repository-specific boundaries a YAML linter cannot know:
# one PR macOS runner, one paired build, thin PR packaging, full master
# packaging, and preservation of every behavior the removed simulator ran.

set -euo pipefail

SCRIPT_PATH=${BASH_SOURCE[0]}
SCRIPT_DIR=${SCRIPT_PATH%/*}
[[ $SCRIPT_DIR != "$SCRIPT_PATH" ]] || SCRIPT_DIR=.
source "$SCRIPT_DIR/lib/require-commands.sh" || exit 2
require_commands find grep sort || exit 2

if [[ -n ${1:-} ]]; then
  ROOT=$1
else
  require_commands git || exit 2
  ROOT=$(git rev-parse --show-toplevel)
fi
WORKFLOW_DIR="$ROOT/.github/workflows"
MACOS_WORKFLOW="$WORKFLOW_DIR/macos-qualification.yml"
CI_WORKFLOW="$WORKFLOW_DIR/ci.yml"
PROVIDER_WORKFLOW="$WORKFLOW_DIR/nip46-provider.yml"
OLD_IOS_WORKFLOW="$WORKFLOW_DIR/ios-simulator-nip11.yml"
BOUNDED_RELAY_TEST="$ROOT/Packages/NMP/Tests/NMPTests/BoundedRelayTimeSharingTests.swift"
CONTROLLED_RELAY="$ROOT/Packages/NMP/Tests/NMPTests/ControlledRelayHarness.swift"
RELAY_INFORMATION_TEST="$ROOT/Packages/NMP/Tests/NMPTests/RelayInformationTests.swift"
FALSIFIER_PROJECT="$ROOT/apps/Falsifier/project.yml"

fail() {
  echo "macOS CI throughput contract: $*" >&2
  exit 1
}

require_file() {
  [[ -f "$1" ]] || fail "missing required file: ${1#"$ROOT/"}"
}

require_text() {
  local file=$1
  local text=$2
  grep -Fq -- "$text" "$file" ||
    fail "${file#"$ROOT/"} is missing required text: $text"
}

forbid_text() {
  local file=$1
  local text=$2
  if grep -Fq -- "$text" "$file"; then
    fail "${file#"$ROOT/"} retains forbidden text: $text"
  fi
}

require_triggers() {
  local file=$1
  require_text "$file" "  push:"
  require_text "$file" "    branches: [master]"
  require_text "$file" "  pull_request:"
  forbid_text "$file" "workflow_dispatch:"
}

require_file "$MACOS_WORKFLOW"
require_file "$CI_WORKFLOW"
require_file "$PROVIDER_WORKFLOW"
require_file "$BOUNDED_RELAY_TEST"
require_file "$CONTROLLED_RELAY"
require_file "$RELAY_INFORMATION_TEST"
require_file "$FALSIFIER_PROJECT"
[[ ! -e "$OLD_IOS_WORKFLOW" ]] ||
  fail "the standalone iOS macOS workflow still exists"
require_triggers "$MACOS_WORKFLOW"

macos_job_count=0
macos_job_owner=
while IFS= read -r workflow; do
  if ! grep -Fq "  pull_request:" "$workflow"; then
    continue
  fi
  workflow_count=$(
    grep -E -c \
      '^[[:space:]]+runs-on:[[:space:]]+macos-[^[:space:]]+[[:space:]]*$' \
      "$workflow" || true
  )
  if [[ "$workflow_count" -gt 0 ]]; then
    macos_job_count=$((macos_job_count + workflow_count))
    macos_job_owner=$workflow
  fi
done < <(
  find "$WORKFLOW_DIR" -maxdepth 1 -type f \
    \( -name '*.yml' -o -name '*.yaml' \) -print |
    sort
)
[[ "$macos_job_count" -eq 1 ]] ||
  fail "expected exactly one PR macOS job, found $macos_job_count"
[[ "$macos_job_owner" == "$MACOS_WORKFLOW" ]] ||
  fail "the one PR macOS job is not owned by macos-qualification.yml"

require_text "$MACOS_WORKFLOW" "name: macOS qualification"
require_text "$MACOS_WORKFLOW" "  macos-qualification:"
require_text "$MACOS_WORKFLOW" "    name: macOS qualification"
require_text "$MACOS_WORKFLOW" '  group: ${{ github.workflow }}-${{ github.event_name }}-${{ github.event.pull_request.number || github.ref }}'
require_text "$MACOS_WORKFLOW" "  cancel-in-progress: true"
macos_named_step_count=$(grep -E -c '^[[:space:]]+- name:' "$MACOS_WORKFLOW" || true)
[[ "$macos_named_step_count" -eq 8 ]] ||
  fail "expected exactly eight named Apple qualification steps, found $macos_named_step_count"

# One resolution unit produces both Apple components. Pull requests need only
# the macOS host architecture exercised by SwiftPM/XCTest; master remains the
# least-frequent trustworthy full package/inventory gate.
require_text "$MACOS_WORKFLOW" "      - name: Select thin PR or full master Apple scope"
require_text "$MACOS_WORKFLOW" 'if [[ "$EVENT_NAME" == pull_request ]]; then'
require_text "$MACOS_WORKFLOW" 'echo "mode=--macos-only"'
require_text "$MACOS_WORKFLOW" 'echo "targets=aarch64-apple-darwin"'
require_text "$MACOS_WORKFLOW" 'echo "mode="'
require_text "$MACOS_WORKFLOW" 'echo "targets=aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios aarch64-apple-darwin"'
paired_build_count=$(
  grep -F -c "scripts/build-swift-nip46-xcframework.sh" "$MACOS_WORKFLOW" || true
)
[[ "$paired_build_count" -eq 1 ]] ||
  fail "expected exactly one paired Apple component build, found $paired_build_count"
forbid_text "$MACOS_WORKFLOW" "scripts/build-swift-xcframework.sh"
forbid_text "$MACOS_WORKFLOW" "--sim-only"

# Core Swift package and public host-XCTest behavior.
require_text "$MACOS_WORKFLOW" "      - name: Build the Swift package"
require_text "$MACOS_WORKFLOW" "      - name: Test the Swift package"
require_text "$MACOS_WORKFLOW" "        run: swift build"
require_text "$MACOS_WORKFLOW" "working-directory: Packages/NMP"
swift_test_count=$(grep -F -c "        run: swift test" "$MACOS_WORKFLOW" || true)
[[ "$swift_test_count" -eq 2 ]] ||
  fail "expected Swift tests for core and NIP-46, found $swift_test_count"

# Optional Swift NIP-46 provider from the same build.
require_text "$MACOS_WORKFLOW" "scripts/check-nip46-component-identity.sh"
require_text "$MACOS_WORKFLOW" "--matched-only"
require_text "$MACOS_WORKFLOW" "scripts/check-nip46-artifact-inventory.sh"
require_text "$MACOS_WORKFLOW" "      - name: Prove identity and clean inventory for every packaged slice"
require_text "$MACOS_WORKFLOW" 'relative_path=${matched_core#Packages/NMP/NMP.xcframework/}'
require_text "$MACOS_WORKFLOW" 'slice_directory=${relative_path%/libnmp_ffi.a}'
require_text "$MACOS_WORKFLOW" "      - name: Test the selectable NIP-46 provider package"
require_text "$MACOS_WORKFLOW" "working-directory: Packages/NMPNip46"

# Simulator-only orchestration is removed because it owned no unique product
# behavior. The unique #598 proof runs through the public Swift API on the host;
# the NIP-11 success/error cases remain in the existing host suite.
forbid_text "$MACOS_WORKFLOW" "xcodegen"
forbid_text "$MACOS_WORKFLOW" "simctl"
forbid_text "$MACOS_WORKFLOW" "xcodebuild test"
forbid_text "$MACOS_WORKFLOW" "pick-ios-simulator-destination.py"
forbid_text "$MACOS_WORKFLOW" "cargo test"
forbid_text "$MACOS_WORKFLOW" "cargo clippy"
forbid_text "$MACOS_WORKFLOW" "cargo fmt"
forbid_text "$MACOS_WORKFLOW" "gradlew"
forbid_text "$FALSIFIER_PROJECT" "FalsifierTests:"
require_text "$BOUNDED_RELAY_TEST" "final class BoundedRelayTimeSharingTests"
require_text "$BOUNDED_RELAY_TEST" "testDurableAutoRoutedWriteProgressesPastAwaitingRelay"
require_text "$BOUNDED_RELAY_TEST" "peakActiveWebSockets"
require_text "$BOUNDED_RELAY_TEST" "testEventDrivenWaitersWithdrawExactlyOnTimeout"
require_text "$CONTROLLED_RELAY" '"NMP Swift Test Relay"'
require_text "$RELAY_INFORMATION_TEST" "testPublicAsyncCallSuspendsMainActorAndDeliversSuccess"
require_text "$RELAY_INFORMATION_TEST" "testPublicAsyncCallDeliversTypedAcquisitionError"

forbid_text "$CI_WORKFLOW" "  swift-package:"
forbid_text "$CI_WORKFLOW" "runs-on: macos-"
require_text "$CI_WORKFLOW" "  surface-regeneration:"
require_text "$CI_WORKFLOW" "  test:"
require_text "$CI_WORKFLOW" "  kotlin-package:"
require_triggers "$CI_WORKFLOW"

forbid_text "$PROVIDER_WORKFLOW" "  swift-provider:"
forbid_text "$PROVIDER_WORKFLOW" "runs-on: macos-"
require_text "$PROVIDER_WORKFLOW" "  package-removal:"
require_text "$PROVIDER_WORKFLOW" "    name: NIP-46 package removal"
require_text "$PROVIDER_WORKFLOW" "  kotlin-provider:"
require_text "$PROVIDER_WORKFLOW" "    name: Kotlin NIP-46 component"
require_text "$PROVIDER_WORKFLOW" 'libnmp_ffi-core-only.so'
require_text "$PROVIDER_WORKFLOW" "scripts/check-nip46-component-identity.sh"
forbid_text "$PROVIDER_WORKFLOW" "--matched-only"
require_triggers "$PROVIDER_WORKFLOW"

echo "macOS CI throughput contract: one paired thin PR job, full master packaging, and host behavior preserved"
