# Issue #637 result manifest

- Harness commit: `34df2528242a9d2227bdbf90662210d62b299b6b`
- Date: 2026-07-17
- Host: `kind2-linux-x86_64`
- Corpus: `/dev/shm/nmp-ndb-100k/corpus.jsonl`
- Corpus BLAKE3: `597105545b9904afd14a44f60695a3edc90876b68c182d8487c7788f8e06efa2`
- Corpus events: 100,000
- Transaction batch size: 4,096
- Repetitions: 11 per layout, alternating order

`redb-index-layout-matrix.json` records `git_dirty: false`, the harness commit,
corpus identity, per-run process metrics, physical sizes, logical keyspace
counts, and exact reopen results.

Command:

```sh
cargo run -q -p nmp-store --release \
  --features bench-instrumentation \
  --example redb_index_layout -- \
  matrix \
  /dev/shm/nmp-ndb-100k/corpus.jsonl \
  benchmarks/nostrdb-compare/results/2026-07-17/issue-637/redb-index-layout-matrix.json \
  4096 11
```

Validation commands:

```sh
cargo test -p nmp-store --features bench-instrumentation
cargo clippy -p nmp-store --features bench-instrumentation --all-targets -- -D warnings
scripts/check-sdk-parity.sh
scripts/check-falsifier-honesty.sh origin/master HEAD
```
