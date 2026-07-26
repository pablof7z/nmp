#!/usr/bin/env bash
# #818: run the Fjall journal-write-error falsifier.
#
# Deterministic entry point for CI and for humans. The regression lives in a
# workspace detached from NMP's build (`tools/fjall-journal-fault/`), because it
# links three mutually exclusive Fjall releases and must never enter the
# production or default feature graph. `cargo test --workspace` does not reach
# it, so without this command the regression would silently rot.
#
# Passing qualifies exactly one behaviour of the pinned Fjall 3.1.8 build: an
# acknowledged transaction is not silently unrecoverable when the journal write
# fails. It does not qualify Fjall semantics, maintenance, performance, or
# production readiness, and it does not select a database. Redb remains the
# production backend.
set -euo pipefail

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
HARNESS="$REPO/tools/fjall-journal-fault/harness"

# The fault is a real RLIMIT_FSIZE/SIGXFSZ filesystem write failure. Linux is
# the lane that is verified to write up to the soft limit and then fail the next
# write(2) with EFBIG while raising SIGXFSZ on the writing thread. The tests
# type other platforms as unsupported rather than skipping silently, but CI must
# run the real fault, so refuse to report success off the supported lane.
if [[ "$(uname -s)" != "Linux" ]]; then
  echo "error: the #818 journal-write fault regression executes its real fault only on Linux;" >&2
  echo "       this host is $(uname -s). Run it on the supported CI lane." >&2
  exit 1
fi

echo "==> fjall journal-write-error falsifier (#818)"
echo "    harness: $HARNESS"

# These packages are detached workspaces, so the repo-wide `cargo fmt --all` and
# `cargo clippy --workspace` in the main CI job never see them. Lint them here or
# they drift.
for package in v3_1_6 v3_1_7 v3_1_8 harness; do
  (
    cd "$REPO/tools/fjall-journal-fault/$package"
    cargo fmt --check
    cargo clippy --locked --all-targets -- -D warnings
  )
done

# `--locked` is load-bearing twice over: it keeps each release probe pinned to
# the exact crate identities recorded in #818, and it makes crate substitution
# fail the run rather than quietly re-resolve.
cd "$HARNESS"
cargo test --locked -- --nocapture

echo "==> ok: 3.1.6 acknowledges the failed journal write; 3.1.7 and 3.1.8 return it"
