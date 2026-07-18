# Issue #696 evidence manifest

## Raw result

- `canonical-runs-matrix.json`
- SHA-256:
  `f3f51aaf0a024007e6a591609e0766e1449347710080d24d0acc717ffdccae3f`
- Schema: `nmp-packed-postings-v4`
- Command: `canonical-runs-matrix`
- Source commit: `beaa5d65f2f7f7179111fef33b9b596d8d6c8e0e`
- Host: `kind2-linux-x86_64`
- Runs: 20 clean fresh-process children, 10 alternating pairs.

## Qualification commands

```text
cargo fmt --all
CARGO_TARGET_DIR=/home/pablo/Work/nmp/target cargo test -p nmp-store --features bench-instrumentation canonical_run_bench
CARGO_TARGET_DIR=/home/pablo/Work/nmp/target cargo test -p nmp-store --features bench-instrumentation committed_segment_survives_abrupt_exit_and_staged_generation_does_not -- --nocapture
CARGO_TARGET_DIR=/home/pablo/Work/nmp/target cargo clippy -p nmp-store --features bench-instrumentation --example packed_postings -- -D warnings
CARGO_TARGET_DIR=/home/pablo/Work/nmp-plan-issue-688-commit-projection-pipeline/target cargo build -p nmp-store --release --features bench-instrumentation --example packed_postings
/home/pablo/Work/nmp-plan-issue-688-commit-projection-pipeline/target/release/examples/packed_postings canonical-runs-matrix /dev/shm/nmp-627-representative-100k.jsonl /tmp/nmp-696-final-matrix-v2.json 10 4096
```

The benchmark adapter was committed before the matrix so every child could
record an exact clean source commit. It is reverted in the final evidence-only
PR because the candidate missed the selection gate.
