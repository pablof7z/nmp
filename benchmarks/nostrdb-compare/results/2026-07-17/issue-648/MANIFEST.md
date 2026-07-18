# Issue #648 result manifest

Date: 2026-07-17  
Candidate commit: `6b1aa6cb6e7c6ec364a61f438176acd6f1a149e8`  
Host: `kind2-linux-x86_64`  
Transaction batch: `4096` events  
Durability: Redb commit; Fjall `PersistMode::SyncAll`

## Corpora

Representative 100k:

- path during measurement: `/dev/shm/nmp-627-representative-100k.jsonl`
- bytes: `66,245,857`
- BLAKE3: `5eb48a3d4e4d051619c9f6656eed697dd1c1bf8eb210de5f9211ec7c0178ad36`

Representative 1m:

- path during measurement: `/dev/shm/nmp-648-representative-1m.jsonl`
- bytes: `662,460,577`
- BLAKE3: `0c2d350b427bf014abf0157806d1d10ebd15a1159a444065e62d2b4c1a78cf9b`
- generated from `private-free-shapes.json` with
  `relay_ingest_bench --events 1000000 --corpus-output ...`

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
  benchmarks/nostrdb-compare/results/2026-07-17/issue-648/packed-postings-matrix.json \
  5 4096
```

Each matrix child used a new temporary database and recorded a clean tree and
the exact candidate commit. Layout order alternated on odd repetitions.

## One-million proof

```sh
target/release/examples/packed_postings run \
  /dev/shm/nmp-648-representative-1m.jsonl \
  packed_redb 4096 0 0
```

## Verification

```sh
cargo test -p nmp-store --features bench-instrumentation
cargo clippy -p nmp-store --all-targets \
  --features bench-instrumentation -- -D warnings
git diff --check
```

All passed. The crate run contained `125` passing library tests, `1` ignored
corpus-cost test, `14` lane-contract tests, `64` outbox-contract tests, and
`45` store-contract tests.

## SHA-256

```text
bb21beeba1328b9fa33931d92c173881cbf44b30a99dfa1a866c72a3d8553670  packed-postings-matrix.json
70d08350dc1fe32709a07583573b140c6f0cce0efdeb74cd70abf60206a5cb4f  packed-redb-1m.json
```
