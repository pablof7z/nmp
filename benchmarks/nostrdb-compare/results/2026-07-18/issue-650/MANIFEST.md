# Issue #650 result manifest

Date: 2026-07-18  
Host: `kind2-linux-x86_64`  
Database filesystem: `/dev/md1` ext4  
Baseline commit: `d4618b58e90d8590646ddd914eb00f8aafed6222`  
Candidate performance commit: `f03dbc66cf523e997aa04b99fe0c0f6d96d6ff9d`  
Candidate evidence head: `f0afce3016b7861fed0a9d8ec2246fa08154f362`

The evidence-head delta adds test-only process-death seams and warning cleanup;
it does not alter the release benchmark path. Copied executable SHA-256 values
are the authoritative cross-commit provenance because each child report's
embedded commit field records its runtime working directory.

## Production pipeline

Baseline binary SHA-256:
`07ff2b6fc8999c54f0ca8d88cacacfbdd0c941a843aadb841c87281a74483c13`

Candidate binary SHA-256:
`1195791375e3d28bfd6005e8d31b3c6e1fa13342871cafabf99775c363f124f0`

Shape source SHA-256:
`83f6fe1ec2947471b4754f42749ca5df5525bf8490fb8c67286b78a7bf55de72`

```sh
relay_ingest_bench \
  --events 100000 \
  --queue-capacity 8192 \
  --verified-cache-capacity 131072 \
  --verifier-workers 8 \
  --verify-batch-size 512 \
  --engine-batch-size 4096 \
  --engine-batch-bytes 8388608 \
  --timeout-secs 240 \
  --shape-corpus benchmarks/nostr-event-corpus/results/2026-07-17/private-free-shapes.json \
  --output FRESH-REPORT.json
```

Twenty pairs alternated order: odd pairs baseline then candidate, even pairs
candidate then baseline. Every child was a fresh process and used a fresh
temporary Redb store. The committed matrix nests all 40 unmodified child
reports and records implementation, pair, and within-pair ordinal.

The decision statistic is the median of the 20 per-pair candidate/baseline
ratios. The independent-median view divides the candidate median by the
baseline median. The bootstrap note in `SUMMARY.md` uses 200,000 resamples of
the 20 throughput ratios with deterministic seed `650` and percentile bounds.

## Selective queries

Baseline binary SHA-256:
`ea63c1a27586502a488e330353ff94a4c779e9b233812af44f19fa2e0e670f72`

Candidate binary SHA-256:
`adbef9442909c41e21de0a3e76da3497fccb041e92adc795abdc6309b0411aed`

Corpus SHA-256:
`2d0394daba3a9e97e2808b636a12150a72ebbb7b7fbe00945293d7fd53f757c5`

```sh
cardinality_query_bench \
  sampled /dev/shm/nmp-627-representative-100k.jsonl 50
```

Five pairs used the same alternating order. Every query report records 100,000
canonical events, 50 iterations, and exact expected row counts.

## One-million scale and reopen

The production command above was repeated once per exact binary with
`--events 1000000` and a 1,200-second deadline. Each child observed exactly
1,000,000 relay frames and reopened exactly 1,000,000 canonical events. The
wrapper records the exact binary hashes alongside both raw reports.

## Verification

```sh
cargo test -p nmp-store --lib
cargo check -p nmp-store
cargo clippy -p nmp-store --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
git diff --check
```

All passed. The library suite reported 120 passed tests and 1 ignored corpus
test.

## SHA-256

```text
f23d374181ddbc13fd49b32bc0d89155ea2290da4d5ca81b3564592e436f42f5  million-event-reopen.json
0475eab53aa85421131474f0be4c7483ccb1685791ba2bc7f90924755f0f77a7  production-pipeline-matrix.json
381204fdd1dd724fa361d21bdfcedea59f9fb6594aa66bec5dd8d76bd2ab4020  selective-query-matrix.json
```

