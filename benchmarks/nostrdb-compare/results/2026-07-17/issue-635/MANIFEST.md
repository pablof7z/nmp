# Issue #635 result manifest

- Harness commit: `ffa550cfce2bd4547b2b29fe838cd2a6643b41ca`
- Date: 2026-07-17
- Host: `kind2-linux-x86_64`
- Corpus: `/dev/shm/nmp-ndb-100k/corpus.jsonl`
- Corpus BLAKE3: `597105545b9904afd14a44f60695a3edc90876b68c182d8487c7788f8e06efa2`
- Corpus events: 100,000
- Transaction batch size: 4,096

`governed-backend-matrix.json` contains five alternating repetitions per
backend. It records `git_dirty: false`, the harness commit above, the corpus
identity, per-run attribution, host writes, peak RSS, stored bytes, and exact
reopen results.

Command:

```sh
cargo run -q -p nmp-store --release \
  --features bench-instrumentation \
  --example store_decomposition -- \
  fjall-matrix \
  /dev/shm/nmp-ndb-100k/corpus.jsonl \
  benchmarks/nostrdb-compare/results/2026-07-17/issue-635/governed-backend-matrix.json \
  4096 5
```

Validation commands:

```sh
cargo test -p nmp-store --features bench-instrumentation
cargo clippy -p nmp-store --features bench-instrumentation --all-targets -- -D warnings
```
