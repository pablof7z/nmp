# Issue #655 result manifest

Date: 2026-07-18

Candidate commit: `2d352c1041294528a0ac40fea0be107350a067f1`

Host: `kind2-linux-x86_64`

Transaction batch: `4096` events

Durability: Redb commit; Fjall `PersistMode::SyncAll`

## Corpus

- path during measurement: `/dev/shm/nmp-627-representative-100k.jsonl`
- events: `100,000`
- bytes: `66,245,857`
- BLAKE3: `5eb48a3d4e4d051619c9f6656eed697dd1c1bf8eb210de5f9211ec7c0178ad36`

## Build

```sh
cargo build --release -p nmp-store \
  --features bench-instrumentation \
  --example packed_postings
```

## Clean alternating matrix

```sh
target/release/examples/packed_postings matrix \
  /dev/shm/nmp-627-representative-100k.jsonl \
  benchmarks/nostrdb-compare/results/2026-07-18/issue-655/packed-postings-production-format-matrix.json \
  5 4096
```

Every matrix child used a fresh temporary database. The raw file records a
clean tree, the exact candidate commit, exact reopen/query results, and the
alternating layout order.

## Verification

```sh
cargo test -p nmp-store --features bench-instrumentation
cargo clippy -p nmp-store --features bench-instrumentation \
  --all-targets -- -D warnings
git diff --check
```

All passed. The feature-enabled crate run contained `133` passing library
tests, `1` ignored corpus-cost test, `14` lane-contract tests, `64`
outbox-contract tests, and `45` store-contract tests.

## SHA-256

```text
6d5da8ab9deb18ecf04175eb11bef9827f78faa05e162afc5562b19b2813a8dc  packed-postings-production-format-matrix.json
```
