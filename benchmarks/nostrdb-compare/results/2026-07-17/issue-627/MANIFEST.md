# Issue #627 result manifest

Harness commit: `84794829b62be634dbcf80a15803895b4abd6194`  
Date: 2026-07-17  
Database filesystem: `/dev/md1` ext4  
Corpus filesystem: tmpfs

## Commands

Build the comparator and production probe:

```sh
NOSTRDB_DIR=/home/pablo/Work/nostrdb-nmp-bench \
  cargo build --release \
  --manifest-path benchmarks/nostrdb-compare/Cargo.toml

cargo build --release -p nmp-engine \
  --example relay_ingest_bench \
  --features bench-instrumentation
```

Generate and describe the representative signed corpus:

```sh
target/release/examples/relay_ingest_bench \
  --events 100000 \
  --shape-corpus benchmarks/nostr-event-corpus/results/2026-07-17/private-free-shapes.json \
  --corpus-output /dev/shm/nmp-627-representative-100k.jsonl \
  --timeout-secs 240 \
  --output /dev/shm/nmp-627-representative-export.json

NOSTRDB_DIR=/home/pablo/Work/nostrdb-nmp-bench \
  benchmarks/nostrdb-compare/target/release/nmp-nostrdb-compare \
  describe-corpus /dev/shm/nmp-627-representative-100k.jsonl
```

Run the equivalent-engine matrices and forced-exit probe with `TMPDIR` on
`/dev/md1`:

```sh
TMPDIR=/home/pablo/Work/nmp-627-disk-tmp \
  benchmarks/nostrdb-compare/target/release/nmp-nostrdb-compare \
  matrix-prepared /dev/shm/nmp-ndb-100k/corpus.jsonl 3 \
  uniform-disk-matrix.json

TMPDIR=/home/pablo/Work/nmp-627-disk-tmp \
  benchmarks/nostrdb-compare/target/release/nmp-nostrdb-compare \
  matrix-prepared /dev/shm/nmp-627-representative-100k.jsonl 5 \
  representative-disk-matrix.json

TMPDIR=/home/pablo/Work/nmp-627-disk-tmp \
  benchmarks/nostrdb-compare/target/release/nmp-nostrdb-compare \
  crash-probe crash-probe.json
```

The production ceiling uses the common arguments below for five paired runs.
Odd repetitions run Memory then Redb; even repetitions reverse the order.
Redb `--store` paths are on `/dev/md1`; generated corpora remain on tmpfs.

```sh
common=(
  --events 100000
  --queue-capacity 8192
  --verified-cache-capacity 131072
  --verifier-workers 8
  --verify-batch-size 512
  --engine-batch-size 4096
  --engine-batch-bytes 8388608
  --timeout-secs 240
  --shape-corpus benchmarks/nostr-event-corpus/results/2026-07-17/private-free-shapes.json
)

target/release/examples/relay_ingest_bench "${common[@]}" \
  --memory-store --output memory-N.json

target/release/examples/relay_ingest_bench "${common[@]}" \
  --store /home/pablo/Work/nmp-627-ceiling-redb-N.redb --output redb-N.json
```

## SHA-256

```text
a4cbbb1744f965f6896e016ce2f6c29fbca05699064590636a3f7ea549a4ff89  SUMMARY.md
be1579a8b51637386243455acd1ea4963352efcc88e937f1ac02333f69259415  crash-probe.json
1d8a1628210d3c0fd850ebdf4edc40113bb813eab184dd52289459e079e3378a  memory-1.json
086d1bea6c2d6f43e15465020fd3c6c94b9a64654cd29ed7a6c2f31b41e6ce0f  memory-2.json
bd492928cab18b868b6b833b52fd9ad7f701e77a6046765675e6a2936b76a026  memory-3.json
03dc3fe337bca01cb01db1c3c59dc67f39f954388ab4a5c02bdab216e5a7e1d2  memory-4.json
6c0213f63850359db10affb8eaf5ffbc6136e826437524eeb4ba9de9dc511fe1  memory-5.json
1231d9c42332d3578120b42dd116e849985aa406d33e5c7987f5c4bc5bb3a36a  redb-1.json
46d5a315c2f5d7dc51ba736134c3365c53586b011eaa88e844394ce1dabe5e72  redb-2.json
8703237601cab49ec0e92113b8cdc0648975fc86e7cdc046cf6a109217468235  redb-3.json
d77c84887177c4e862a24cc26a9828687b4bc38d07a85a9ec6f03848e3088d3b  redb-4.json
a8126aaec297235fbe05c707194df66d37049383f6575c263979a577381e16e3  redb-5.json
19b929968a5b56be7d78c67af97ec7feb9e5c42de4d97137b00eeb8d40d3c9bc  representative-corpus.meta.json
9a0d86309d3bc48c2c4acc1e15da6743fec43c22979e47ad776887a61c5be42c  representative-disk-matrix.json
215d85852d5fd3d258b139ebd5fa66132c3cc8d9b378cd274a37ccff35183796  uniform-disk-matrix.json
```
