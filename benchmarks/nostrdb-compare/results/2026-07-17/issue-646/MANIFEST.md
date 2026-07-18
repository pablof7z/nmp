# Issue #646 result manifest

Date: 2026-07-17  
Database filesystem: `/dev/md1` ext4  
Corpus filesystem: tmpfs  
Baseline commit: `30ad22b63132a4d08a5096196c8db61373fe88e9`  
Candidate code commit: `0cc53ff2001b1e0a1887c383f8351e887689d210`

Later candidate hashes recorded by query and production children are
evidence-only descendants of the same code commit. Every child records a clean
tree and its exact commit in the raw matrix.

## Builds

```sh
cargo build --release -p nmp-store \
  --features bench-instrumentation \
  --example cardinality_query_bench

cargo build --release -p nmp-engine \
  --features bench-instrumentation \
  --example relay_ingest_bench
```

Both binaries were built in the baseline and candidate worktrees. Repetitions
alternate `baseline,candidate` then `candidate,baseline`; every invocation is a
fresh process.

## Governed import and query children

```sh
target/release/examples/cardinality_query_bench \
  sampled /dev/shm/nmp-627-representative-100k.jsonl 1

target/release/examples/cardinality_query_bench \
  sampled /dev/shm/nmp-627-representative-100k.jsonl 50
```

The first command ran 15 paired repetitions for the import matrix. The second
ran five paired repetitions for the query matrix. Raw child reports were
assembled without changing their measured fields.

## Production pipeline children

```sh
target/release/examples/relay_ingest_bench \
  --events 100000 \
  --queue-capacity 8192 \
  --verified-cache-capacity 131072 \
  --verifier-workers 8 \
  --verify-batch-size 512 \
  --engine-batch-size 4096 \
  --engine-batch-bytes 8388608 \
  --timeout-secs 240 \
  --shape-corpus benchmarks/nostr-event-corpus/results/2026-07-17/private-free-shapes.json \
  --store /home/pablo/Work/nmp-646-clean-pipeline.RUN/BACKEND-N.redb \
  --output /dev/shm/nmp-646-clean-BACKEND-N.json
```

This command ran five paired repetitions. Each child used a new Redb file.

## SHA-256

```text
793d4e2799dd188f961037c1afa583bf962fe18ab05ec3d02d18b064ee9e25c8  author-kind-import-matrix.json
48bd73b42cbf730ee55d153ec0213efd512f675a48bef1961e913f00740bb008  author-kind-query-matrix.json
c205a92f9759418d4059be2f17b225bffdbb4f3273ca843999245a50b8fe541c  author-kind-production-pipeline-matrix.json
```

