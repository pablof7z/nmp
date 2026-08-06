#!/usr/bin/env bash
# Run the base-trusted surface gate and report which outcome it reached.
#
# The gate has three kinds of result and they mean completely different things
# (#1264):
#
#   accepted    the head was judged and passed
#   rejected    the head was judged and rejected -- about the proposed change
#   stale-base  the head is not on the PR's current base, so it was not judged;
#               the branch needs the current base merged in
#   no-verdict  the gate never decided anything: its inputs, its tools, or the
#               toolchain failed. Not about the proposed change at all.
#
# This program exits 0 for the first three -- a verdict exists, whatever it says
# -- and nonzero only for the last. That is what lets the job running it carry
# the single claim "a verdict was rendered", so a broken gate can never turn red
# under the check name that means "your change was rejected". The rejection
# itself is rendered by a separate downstream job reading the outcome below.
#
# Nothing here is fail-open: `no-verdict` exits nonzero and blocks. The point is
# only that it blocks under its own name.
set -euo pipefail

MALFUNCTION_EXIT=70
STALE_BASE_EXIT=4

note() { echo "report-surface-governance-verdict: $*" >&2; }

status=0
if [[ $# -lt 1 ]]; then
  note "usage: $0 <gate-program> [argument...]"
  status=$MALFUNCTION_EXIT
elif [[ ! -f $1 || ! -x $1 ]]; then
  # An executable file by path: never a name resolved through PATH, never a
  # shell function inherited from the environment. The gate this program reports
  # on must be the gate that actually ran, which is why the argument is checked
  # rather than trusted. That reason holds on every shell and is on its own the
  # justification for this branch.
  #
  # It also closes a narrower hole, measured rather than assumed. The status
  # bash reports for an unrunnable program is not the same everywhere:
  #
  #   missing file        bash 3.2.57 + set -e -> 1     bash 5.3.15 -> 127
  #   exists, not +x      bash 3.2.57 + set -e -> 1     bash 5.3.15 -> 126
  #   missing, no set -e  bash 3.2.57          -> 127   bash 5.3.15 -> 127
  #
  # This script sets `set -e`, and 1 is the code that means "the head was
  # rejected". So on macOS's /bin/bash -- which runs the falsifier suite locally
  # and the by-hand signoff in protected-path-signoff.md 2.1 -- an unrunnable
  # gate would have rendered as a verdict. On the ubuntu-latest runner it would
  # not: 127 and 126 already fall through to no-verdict. Checking first makes
  # the classification independent of which bash is reading it.
  note "the gate program is not an executable file: $1"
  status=$MALFUNCTION_EXIT
else
  # The status is captured from the command itself and never through a pipe. A
  # pipe reports the last stage's status, and `${PIPESTATUS[0]}` is empty under
  # some of the shells this repository is read in -- either way the gate's real
  # result would be lost.
  "$@" || status=$?
fi

case "$status" in
  0) outcome=accepted ;;
  1) outcome=rejected ;;
  "$STALE_BASE_EXIT") outcome=stale-base ;;
  *) outcome=no-verdict ;;
esac

printf 'surface-governance-outcome: %s (gate exit %s)\n' "$outcome" "$status"
if [[ -n ${GITHUB_OUTPUT:-} ]]; then
  printf 'outcome=%s\n' "$outcome" >> "$GITHUB_OUTPUT" || {
    note "the outcome could not be recorded for the reporting job"
    exit "$MALFUNCTION_EXIT"
  }
fi

[[ $outcome == no-verdict ]] || exit 0
note "the gate exited $status without reaching a verdict; this is a statement about the gate, not about the proposed change"
exit "$MALFUNCTION_EXIT"
