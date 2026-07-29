#!/usr/bin/env bash
# #1007: a repository gate must not report success when a search/build tool
# it needs is absent. These falsifiers remove tools from PATH for real; they
# do not replace a missing tool with a successful stub.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
BASH_BIN=$(command -v bash)
TMP=$(mktemp -d "${TMPDIR:-/tmp}/nmp-check-tools.XXXXXX")
trap 'rm -rf "$TMP"' EXIT

fail() {
  echo "check-tool-requirements: $*" >&2
  exit 1
}

assert_missing_tool_refused() {
  local checker=$1
  local missing_tool=$2
  shift 2
  local path_dir="$TMP/path-${missing_tool}"
  local log="$TMP/${missing_tool}.log"
  local available_tool

  mkdir "$path_dir"
  for available_tool in "$@"; do
    ln -s "$(command -v "$available_tool")" "$path_dir/$available_tool"
  done

  if PATH="$path_dir" "$BASH_BIN" "$ROOT/$checker" >"$log" 2>&1; then
    fail "$checker passed with $missing_tool absent"
  fi
  grep -Fq "required command(s) unavailable: $missing_tool" "$log" ||
    fail "$checker did not identify absent $missing_tool"
}

# The reported defect: cargo and dirname are real, while rg genuinely cannot
# be resolved from the checker's PATH.
assert_missing_tool_refused \
  scripts/check-content-parser-boundary.sh rg cargo dirname

# A sibling with `grep ... || true` had the same silent-success shape.
assert_missing_tool_refused \
  scripts/check-no-compatibility-surface.sh grep dirname

# Sweep every current checker. With an actually empty PATH, each must stop at
# its declared prerequisites and must not reach a usage branch or an `ok`.
# `check-surface-governance.sh` is the one base-trusted program that an
# ordinary PR is structurally forbidden to edit. Its existing `set -euo
# pipefail` still makes the first absent `git` invocation fatal, so exercise
# that refusal without requiring the shared helper to appear in the file.
mkdir "$TMP/empty-path"
checker_count=0
for checker in "$ROOT"/scripts/check-*.sh; do
  checker_count=$((checker_count + 1))
  checker_name=${checker##*/}
  log="$TMP/$checker_name.log"
  if PATH="$TMP/empty-path" "$BASH_BIN" "$checker" >"$log" 2>&1; then
    fail "$checker_name passed with every external command absent"
  fi
  if [[ $checker_name == check-surface-governance.sh ]]; then
    grep -Fq 'git: command not found' "$log" ||
      fail "$checker_name did not fail at its first absent verification tool"
    continue
  fi
  grep -Fq 'check-tools: required command(s) unavailable:' "$log" ||
    fail "$checker_name reached something other than its prerequisite refusal"
done

((checker_count > 0)) || fail "no scripts/check-*.sh files were exercised"
echo "check-tool-requirements: ok ($checker_count checkers fail closed)"
