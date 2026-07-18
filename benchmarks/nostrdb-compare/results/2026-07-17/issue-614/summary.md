# Equivalent storage-engine ceiling (#614)

This run compares Redb and the pinned nostrdb checkout's LMDB using the same
prevalidated NMP bytes, transaction boundaries, synchronous durability, and
logical table set. It is a benchmark-only comparison; LMDB remains outside
NMP's production dependency graph.

## Controlled work

The harness prepares the corpus before either engine's timed region. Each
engine receives the same ordered sequence of 918,827 records at batch 4,096:

| Table | exact rows after reopen |
| --- | ---: |
| canonical event values | 100,000 |
| event-id lookup | 100,000 |
| relay observations | 100,000 |
| relay dictionary / reverse lookup / refcount | 1 / 1 / 1 |
| global ordered index | 100,000 |
| author ordered index | 100,000 |
| kind ordered index | 100,000 |
| author-kind ordered index | 100,000 |
| single-letter tag index | 125,000 |
| index cardinality buckets | 4,248 |

Keys and values are produced once by NMP's production portable encoder,
observation layout, ordered-index builders, packed tag-index builder, and
cardinality-key builders. Redb applies those records through its production
table definitions. The C bridge gives LMDB the byte-identical records in one
descriptor array per transaction. Both use their synchronous defaults: Redb's
ordinary commit and LMDB without `MDB_NOSYNC` or `MDB_NOMETASYNC`.

All 20 prepared-engine cells reopened with every expected row in all twelve
tables. The five full-governed Redb cells also reopened exactly 100,000
canonical events. The forced process-death probe exited inside a live,
uncommitted transaction for each engine; after reopen, the committed row was
present and the uncommitted row was absent for both.

## Reproduction

Harness commit: `1e35ed166b6f4e95015f5da39711dbb9e3953ebc` (clean during
all matrix children). Pinned nostrdb/LMDB commit:
`f4591db9524bc4936af76af4750ec425e67700be`.

```sh
NOSTRDB_DIR=/home/pablo/Work/nostrdb-nmp-bench \
  cargo run --release \
  --manifest-path benchmarks/nostrdb-compare/Cargo.toml -- \
  matrix-equivalent /dev/shm/nmp-ndb-100k/corpus.jsonl 5 \
  benchmarks/nostrdb-compare/results/2026-07-17/issue-614/equivalent-matrix.json

NOSTRDB_DIR=/home/pablo/Work/nostrdb-nmp-bench \
  benchmarks/nostrdb-compare/target/release/nmp-nostrdb-compare \
  crash-probe \
  benchmarks/nostrdb-compare/results/2026-07-17/issue-614/crash-probe.json
```

The deterministic 100,000-event, 128-byte-payload corpus is 50,610,025 bytes
with BLAKE3
`597105545b9904afd14a44f60695a3edc90876b68c182d8487c7788f8e06efa2`.
The matrix alternates cell order across five fresh-process repetitions, and
every cell uses a fresh database under `/tmp`.

Machine: `kind2`, Linux 6.1.0-42-amd64, Intel Core Ultra 7 265 (20 logical
CPUs), 66,998,853,632 bytes RAM. Corpus input was on tmpfs; database files were
on `/dev/md1` ext4.

## Results

Throughput is the five-run median with the min-max range. Other columns are
five-run medians. Process writes come from `/proc/self/io`.

| path | batch | events/s median (range) | wall ms | commit ms | process writes | allocated DB | reopen ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Redb prepared equivalent | 128 | 7,378 (7,149-9,126) | 13,554.7 | 11,048.5 | 1,386,827,776 | 140,812,288 | 20.92 |
| LMDB prepared equivalent | 128 | 7,341 (6,279-8,165) | 13,622.3 | 12,916.2 | 1,367,547,904 | 116,752,384 | 0.22 |
| Redb prepared equivalent | 4,096 | 72,838 (50,098-84,365) | 1,372.9 | 760.9 | 342,577,152 | 163,987,456 | 13.29 |
| LMDB prepared equivalent | 4,096 | 89,888 (38,212-105,604) | 1,112.5 | 737.8 | 349,532,160 | 144,740,352 | 0.21 |
| full governed Redb | 4,096 | 39,403 (31,689-66,768) | 2,537.9 | 1,441.8 | 341,741,568 | 163,897,344 | n/a |

The batch-4,096 paired LMDB/Redb throughput ratios by repetition are 0.94,
1.25, 1.83, 0.63, and 1.23; the paired median is **1.23x**. At batch 128 the
paired median is **1.00x**. Redb and LMDB have nearly the same batch-4,096
commit median (760.9 versus 737.8 ms), and LMDB writes 2.0% more process bytes.
Its gain comes primarily from lower apply CPU (593 versus 942 ms median), not
from eliminating commit or write amplification.

LMDB's allocated database is 11.7% smaller and its reopen is much faster, but
its absolute peak RSS with the same prepared arena is 622.0 MB versus Redb's
477.1 MB. That is a 30% benchmark-process increase, not a production RSS win.

The governed-path cell is deliberately included because #614 requested it,
but its 31.7k-66.8k spread shows the same host-level Redb variance seen in
#618. #618's dedicated five-run production-path median (66.8k events/s) remains
the production attribution authority. The controlled prepared-engine ratio is
the storage-engine decision evidence here.

## Decision: retain Redb

LMDB does not expose a replacement-sized storage multiplier under equivalent
work:

- its median advantage at the production transaction size is only 1.23x;
- its best of five runs is 105.6k events/s, still 1.42x short of the epic's
  150k initial-ingest gate;
- it does not reduce commit amplification or process writes;
- its benchmark RSS is materially higher;
- the measured candidate is a C library and therefore does not supply the
  credible pure-Rust iOS, Android, desktop, and wasm story required by #614's
  decision rule.

Do not open a storage-replacement architecture decision and do not add LMDB to
the production dependency graph. Keep Redb. The remaining epic gap is not an
engine swap: continue with the post-storage production profile before deciding
whether #613 ownership cleanup or #615 parse/materialization work is justified.

Raw per-run timing, CPU, allocations, RSS, write bytes, database sizes, reopen
counts, commit identities, commands, and cell order are in
`equivalent-matrix.json`; forced-crash results are in `crash-probe.json`.
