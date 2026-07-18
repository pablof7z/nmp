# Issue #691 result manifest

Date: 2026-07-18

Candidate commit: `bca1f5fa22cda43cb13755bcdbd3db7628870d67`

Host: `kind2-linux-x86_64`

Transaction batch: `4096` events

Durability: Redb commit; both Fjall layouts use `PersistMode::SyncAll`

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

## Clean balanced matrix

```sh
target/release/examples/packed_postings one-keyspace-matrix \
  /dev/shm/nmp-627-representative-100k.jsonl \
  benchmarks/nostrdb-compare/results/2026-07-18/issue-691/one-keyspace-fjall-matrix.json \
  10 4096
```

Every child used a fresh temporary database. Even repetitions ran packed Redb,
multi-keyspace Fjall, then one-keyspace Fjall; odd repetitions reversed the
order. The raw file records a clean tree and exact reopen/query success for all
30 runs.

The candidate was implemented and measured at the commit above, then reverted
because it failed the issue gate. Git history preserves the exact adapter.

## Verification

```sh
cargo test -p nmp-store --features bench-instrumentation \
  fjall_one_keyspace_prefixes_isolate_logical_tables_after_reopen
cargo check -p nmp-store --features bench-instrumentation \
  --example packed_postings
git diff --check
```

The focused prefix-isolation/reopen test passed. The release matrix additionally
exercised ingest, deletion overlay, persisted-data compaction, reopen, and the
4 query oracles through every candidate run. The full crate gate applies to the
final evidence-only tree after the rejected adapter is reverted.

## SHA-256

```text
d44f2118eee5d18f9d1a531eccfdba60ac66ddfef035b9bc654eef27cbac0876  one-keyspace-fjall-matrix.json
```
