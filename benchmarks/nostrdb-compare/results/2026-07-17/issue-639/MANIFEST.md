# Issue #639 result manifest

- Harness commit: `b7447391ab32a487a4c9407641679897291b7a32`
- Date: 2026-07-17
- Host: `kind2-linux-x86_64`
- Corpus: `/dev/shm/nmp-ndb-100k/corpus.jsonl`
- Corpus BLAKE3: `597105545b9904afd14a44f60695a3edc90876b68c182d8487c7788f8e06efa2`
- Corpus events: 100,000
- Transaction batch size: 4,096
- Repetitions: 11 paired runs, alternating order

`redb-index-order-matrix.json` records `git_dirty: false`, the harness commit,
corpus identity, staging and database wall time, commit latency, process
metrics, physical sizes, logical keyspace counts, and exact reopen results.

Command:

```sh
cargo run -q -p nmp-store --release \
  --features bench-instrumentation \
  --example redb_index_layout -- \
  order-matrix \
  /dev/shm/nmp-ndb-100k/corpus.jsonl \
  benchmarks/nostrdb-compare/results/2026-07-17/issue-639/redb-index-order-matrix.json \
  4096 11
```

Validation commands:

```sh
cargo test -p nmp-store --features bench-instrumentation
cargo clippy -p nmp-store --features bench-instrumentation --all-targets -- -D warnings
scripts/check-sdk-parity.sh
scripts/check-falsifier-honesty.sh origin/master HEAD
```
