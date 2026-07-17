# Store/commit cost decomposition (#618)

This run measures the production `NmpStore` write path and benchmark-only reduced
Redb table sets on the same deterministic 100,000-event, 128-byte-payload corpus.
It exists to decide which epic #612 unit can still move a multiplier; the reduced
variants are measurement instruments, not proposed production fast paths.

## Reproduction

Harness commit: `b9fd963a656c1f97af97247b8c470079e4b2150a` (clean worktree)

```sh
NOSTRDB_DIR=/home/pablo/Work/nostrdb-nmp-bench \
  cargo run --release \
  --manifest-path benchmarks/nostrdb-compare/Cargo.toml -- \
  generate /dev/shm/nmp-ndb-100k/corpus.jsonl 100000 128

cargo run -p nmp-store --release \
  --features bench-instrumentation \
  --example store_decomposition -- \
  matrix /dev/shm/nmp-ndb-100k/corpus.jsonl \
  benchmarks/nostrdb-compare/results/2026-07-17/issue-618/store-decomposition.json \
  5
```

The corpus is 50,610,025 bytes and has BLAKE3
`597105545b9904afd14a44f60695a3edc90876b68c182d8487c7788f8e06efa2`.
The matrix alternates variant order across five fresh-process repetitions. Every
cell used a fresh database, reopened it after the write, and asserted exactly
100,000 rows. All 80 cells passed.

Machine: `kind2`, Linux 6.1.0-42-amd64, Intel Core Ultra 7 265 (20 logical CPUs),
66,998,853,632 bytes RAM. Corpus input was on tmpfs; temporary Redb databases
were on `/tmp` backed by `/dev/md1` ext4.

Raw per-run wall/CPU/commit time, allocations, RSS, process write bytes, logical
and allocated database bytes, row checks, command, commit, and host identity are
in `store-decomposition.json` beside this file.

## Five-run results

Throughput is the median; the range is min-max. Writes are median bytes reported
by `/proc/self/io`. Stored is Redb's median logical stored bytes.

| Path (4,096-event transactions unless noted) | events/s median (range) | wall ms | commit ms | process writes | stored |
| --- | ---: | ---: | ---: | ---: | ---: |
| encode only | 4,580,702 (4,545,839-6,506,517) | 21.8 | 0.0 | 0 | 0 |
| canonical event + ID | 158,191 (143,181-160,826) | 632.1 | 393.4 | 135,602,176 | 36,665,025 |
| + provenance | 146,609 (91,194-154,425) | 682.1 | 392.1 | 140,046,336 | 38,665,111 |
| + four ordered indexes | 88,385 (65,059-103,547) | 1,131.4 | 596.7 | 216,784,896 | 64,665,111 |
| + tag index | 60,797 (36,379-74,343) | 1,644.8 | 968.4 | 337,158,144 | 72,980,136 |
| + cardinality | 51,449 (28,626-70,217) | 1,943.7 | 1,161.7 | 342,577,152 | 73,083,650 |
| full governed `NmpStore` | 66,828 (34,406-74,287) | 1,496.4 | 713.8 | 341,647,360 | 73,083,720 |

Reduced-table variants deliberately bypass production governance and therefore
cannot be subtracted from the full path as a clean estimate of semantic overhead.
Their high run-to-run Redb variance is preserved in the raw results. The
production instrumentation below is the attribution authority.

### Production-path attribution at batch 4,096

| Phase | median ms | share of median transaction time |
| --- | ---: | ---: |
| commit | 713.8 | 48.0% |
| ordered-index inserts | 360.5 | 24.2% |
| canonical-row inserts | 231.7 | 15.6% |
| governed-semantics residual | 119.0 | 8.0% |
| flush | 53.0 | 3.6% |
| event encoding | 6.7 | 0.45% |
| table open + begin | 1.1 | 0.07% |
| total transaction | 1,487.9 | 100% |

The residual is measured `apply_events` time after event encoding, canonical-row
insertion, and index insertion. These independently timed production phases cover
the complete transaction rather than extrapolating from reduced variants.

### Transaction-size scaling on the full path

| events/transaction | events/s median (range) | commit ms | process writes |
| ---: | ---: | ---: | ---: |
| 128 | 7,823 (7,097-8,641) | 10,106.1 | 1,390,047,232 |
| 256 | 13,341 (12,785-16,518) | 6,075.1 | 980,213,760 |
| 512 | 22,441 (16,193-25,463) | 3,233.3 | 721,747,968 |
| 1,024 | 35,781 (35,478-41,212) | 1,736.1 | 544,591,872 |
| 2,048 | 45,393 (30,238-51,299) | 1,167.5 | 421,949,440 |
| 4,096 | 66,828 (34,406-74,287) | 713.8 | 341,647,360 |

Moving 128 to 4,096 events per transaction improves the median 8.54x, reduces
commit time 92.9%, and reduces process writes 75.4%. The ordered assembler merged
for #616 already uses the 4,096-event bound, so transaction coalescing is no
longer the unclaimed multiplier.

At 4,096, Redb causes 341.6 MB of process writes for 73.1 MB stored (4.67x).
Adding the tag index to the other ordered indexes adds 120.4 MB of process writes
(+55.5%) for 8.3 MB of stored data (+12.9%) and lowers median throughput 31.2%.

## Decision

The epic's 150,000 events/s gate is 2.24x above the full-path median. Encoding is
only 0.45% of production transaction time, so store re-encoding is not the next
multiplier. JSON parsing, signature verification, and initial owned-event
materialization happen before this harness's prevalidated store boundary; this
result therefore does not close #615. Keep that prototype behind #614 and use a
post-storage production profile to decide whether its broader packed parse path
is justified. Allocation cleanup in #613 can still improve memory and CPU, but
this store-only ceiling shows it is not the primary throughput lever.

Proceed with #614: compare an equivalent required index set and exact reopen
semantics against a different storage representation/engine, with special focus
on Redb commit amplification and the tag-index physical layout. Keep every NMP
semantic and index intact; the benchmark-only reduced variants do not authorize
removing observable behavior.
