# Issue #629 result manifest

Harness commit: `b05f1e31f314ffb0d194b9ed54f5cfaaf4ff0b5d`  
Date: 2026-07-17  
Corpus: `/dev/shm/nmp-627-representative-100k.jsonl`  
Corpus BLAKE3: `5eb48a3d4e4d051619c9f6656eed697dd1c1bf8eb210de5f9211ec7c0178ad36`  
Database filesystem: `/dev/md1` ext4 via task-specific `TMPDIR`  
Transaction batch size: 4096

Every checked-in JSON records `git_dirty: false`, the harness commit above,
exact table counts, and `exact_reopen: true`.

`exact_reopen` is the historical harness field name. Here it means only that
the expected physical row counts were present after reopen; it does not prove
exact semantic recovery. No winner/provenance/tombstone/coverage/outbox/receipt
oracle ran in this benchmark.

Build:

```sh
NOSTRDB_DIR=/home/pablo/Work/nostrdb-nmp-bench \
  cargo build --release \
  --manifest-path benchmarks/nostrdb-compare/Cargo.toml
```

Five-cycle commands were alternated Redb/Fjall by repetition:

```sh
nmp-nostrdb-compare run-sustained \
  /dev/shm/nmp-627-representative-100k.jsonl \
  fjall-balanced-prepared 4096 5 output.json

nmp-nostrdb-compare run-sustained \
  /dev/shm/nmp-627-representative-100k.jsonl \
  redb-prepared 4096 5 output.json
```

The same commands used `10` cycles for the longer paired check. The decisive
set is 3 five-cycle results per backend plus 1 ten-cycle result per backend.
