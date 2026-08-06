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
  # An executable file by path, never a name resolved through PATH and never a
  # shell function inherited from the environment. Two reasons: the gate this
  # program reports on must be the gate that ran, and a command bash cannot
  # find does not report a stable status -- bash 3.2 returns 1 for it, which is
  # the code that means "the head was rejected". A missing program would then
  # have rendered as a verdict, which is the exact defect this file exists to
  # remove.
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
