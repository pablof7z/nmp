#!/usr/bin/env bash
# Structural contract for issue #1050. actionlint owns generic workflow syntax;
# this checker owns the repository-specific invariants a YAML linter cannot
# know: one PR macOS runner, three preserved suites, and an isolated key.

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

fail() {
  echo "macOS CI throughput contract: $*" >&2
  exit 1
}

require_file() {
  [[ -f "$1" ]] || fail "missing workflow: ${1#"$ROOT/"}"
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

# Suite 1: clean-clone Swift package.
require_text "$MACOS_WORKFLOW" "scripts/build-swift-xcframework.sh --sim-only"
require_text "$MACOS_WORKFLOW" "      - name: Build the Swift package"
require_text "$MACOS_WORKFLOW" "      - name: Test the Swift package"
require_text "$MACOS_WORKFLOW" "        run: swift build"
require_text "$MACOS_WORKFLOW" "working-directory: Packages/NMP"
swift_test_count=$(grep -F -c "        run: swift test" "$MACOS_WORKFLOW")
[[ "$swift_test_count" -eq 2 ]] ||
  fail "expected Swift tests for core and NIP-46, found $swift_test_count"

# Suite 2: optional Swift NIP-46 provider.
require_text "$MACOS_WORKFLOW" "scripts/build-swift-nip46-xcframework.sh --sim-only"
require_text "$MACOS_WORKFLOW" "scripts/check-nip46-component-identity.sh"
require_text "$MACOS_WORKFLOW" "scripts/check-nip46-artifact-inventory.sh"
require_text "$MACOS_WORKFLOW" "      - name: Test the selectable NIP-46 provider package"
require_text "$MACOS_WORKFLOW" "working-directory: Packages/NMPNip46"

# Suite 3: iOS Simulator NIP-11 runtime.
require_text "$MACOS_WORKFLOW" "scripts/pick-ios-simulator-destination.py"
require_text "$MACOS_WORKFLOW" "xcodebuild test (iOS Simulator NIP-11 runtime qualification)"
require_text "$MACOS_WORKFLOW" "          xcodebuild test \\"
require_text "$MACOS_WORKFLOW" "-test-iterations 3"

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
require_triggers "$PROVIDER_WORKFLOW"

echo "macOS CI throughput contract: one stable job owns all three suites"
