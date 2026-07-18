# Issue #658 result manifest

- Date: 2026-07-18
- Host: `kind2-linux-x86_64`
- Host CPU: Intel Core Ultra 7 265, 20 logical CPUs
- Kernel: Linux 6.1.0-42-amd64
- Database filesystem: `/dev/md1` ext4, `noatime`
- NMP base commit: `d94399a71009deedfd221daceb23863a2c020579`
- Rust: `rustc 1.99.0-nightly (da80ed070 2026-07-14)`
- Redb: 4.1.0
- heed: 0.22.1
- lmdb-master-sys: 0.2.6

The evidence binaries were built from the dirty #658 worktree because the
benchmark-only seams and LMDB adapter are the subject of this PR. Exact binary
hashes below are the executable provenance. The post-measurement source delta
replaces the benchmark lifecycle boolean with the architecture-gate-required
enum, removes a redundant explicit drop flagged by Clippy, and adds tests and
documentation. It does not change the measured transaction work, codecs,
compaction, durability, or verification path.

## Redb durability ceiling

Binary SHA-256:
`0e0d88300bbf0d009037c2c6c5d06594aac9d56c97cf8b22aa83fab5410a4673`

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
  [--redb-nondurable-diagnostic] \
  --store FRESH.redb \
  --output FRESH.json
```

Ten pairs used fresh child processes and fresh stores. Odd pairs ran Immediate
first; even pairs ran the non-durable diagnostic first. The diagnostic's final
Immediate checkpoint is timed in `durability_checkpoint_ns`; the raw matrix
nests all 20 unmodified probe reports plus pair and ordinal metadata.

The syscall attribution repeated each mode under:

```sh
strace -f -c -e trace=fsync,fdatasync,msync \
  -o SYNC-SUMMARY.txt relay_ingest_bench [arguments above]
```

The Immediate diagnostic observed 46 `fdatasync` calls totaling 0.142333 s.
The non-durable-plus-checkpoint diagnostic observed 17 calls totaling 0.056015
s. `strace` changed batching and absolute wall time, so these runs attribute the
syscall cohort only; the untraced alternating matrix owns performance claims.

## LMDB ceiling

Binary SHA-256:
`666a7c1c40025cdcce393331870273e8620b27e5bd5d50cb6db393f09e3a9318`

Corpus:

- path during measurement: `/dev/shm/nmp-627-representative-100k.jsonl`
- events: 100,000
- bytes: 66,245,857
- BLAKE3: `5eb48a3d4e4d051619c9f6656eed697dd1c1bf8eb210de5f9211ec7c0178ad36`
- SHA-256: `2d0394daba3a9e97e2808b636a12150a72ebbb7b7fbe00945293d7fd53f757c5`

```sh
cargo build -p nmp-store --release \
  --features bench-instrumentation \
  --example packed_postings

target/release/examples/packed_postings ceiling-matrix \
  /dev/shm/nmp-627-representative-100k.jsonl \
  FRESH-MATRIX.json \
  10 4096
```

The exact binary ran 2 consecutive ten-pair matrices; the committed raw matrix
combines them as repetitions 0–19 without altering child metrics. Every child
used a fresh temporary database. Pair order alternated Redb/LMDB and LMDB/Redb.

LMDB opened one 16 GiB virtual map (sparse, not preallocated), 32 maximum named
databases, and no unsafe/no-sync flags. The adapter used 26 named databases and
one synchronous write transaction per governed batch.

## Verification

```sh
cargo test -p nmp-store --features bench-instrumentation --lib
cargo test -p nmp-engine --features bench-instrumentation \
  --test relay_ingest_smoke
cargo check -p nmp-store
cargo check -p nmp-store --features bench-instrumentation
cargo check -p nmp-store --features bench-instrumentation \
  --example packed_postings
cargo clippy -p nmp-store --features bench-instrumentation \
  --all-targets -- -D warnings
cargo fmt --all -- --check
git diff --check
```

All commands passed. `nmp-store` reported 264 passing tests and 1 ignored
corpus-cost test across its library and contract suites. The complete ordinary
`nmp-engine` crate suite passed; the feature-enabled diagnostic smoke added by
this issue also passed. The first default-debuginfo engine attempt exhausted
the host disk while linking unrelated integration binaries; rerunning the same
suite with `CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0` passed and changes
only build-artifact size.

## SHA-256

```text
c149348679ef43560f5c0dce12c331cf7e0d88891dfdcd3b37978dd9f60c3f5d  lmdb-ceiling-matrix.json
14877d0829c11ea26f962c43015b6d6ddfb1e13fd56251f8bf974822fae4c283  redb-durability-matrix.json
```
