# Issue #629 — bounded Fjall maintenance evidence

This checkpoint asks whether Fjall retains issue #627's foreground throughput
and host-write advantage after explicit memory and thread constraints expose
LSM maintenance. It does **not** prove a production `EventStore` migration:
governance, real queries, outbox state, crash seams, and platform packaging
remain outside this prepared physical-store harness.

## Qualified profile

The compared Fjall 3.1.6 profile uses:

- 2 maintenance workers;
- 16 MiB block cache;
- requested 32 MiB aggregate write buffer; and
- 4 MiB per-keyspace memtables across the 12 equivalent NMP keyspaces.

The aggregate ceiling is a hidden, deprecated Fjall 3.1.6 API. The harness
therefore records actual ending buffer bytes and L0/SST counts. An exploratory
1-worker run ended with 103–144 MiB buffered and failed boundedness; 2 workers
kept the clean repeated runs at 23–41 MiB.

## Sustained update-heavy result

`run-sustained` repeats the exact prepared 100k physical record set. Later
cycles overwrite the same keys, deliberately stressing LSM version retention
and compaction rather than modeling ordinary append-only relay ingest.

Three clean-tree alternating five-cycle runs (500k logical mutations), commit
`b05f1e31f314ffb0d194b9ed54f5cfaaf4ff0b5d`:

| Metric | Fjall, 2 workers | Redb | Difference |
| --- | ---: | ---: | ---: |
| Median throughput | 26,949/s | 19,877/s | Fjall 1.356x |
| Throughput range | 26,092–27,689/s | 18,227–26,894/s | — |
| Commit p50 | 116.0 ms | 106.9 ms | Fjall 8.5% slower |
| Commit p95 | 184.6 ms | 264.2 ms | Fjall 30.1% lower |
| Commit p99 | 262.5 ms | 322.0 ms | Fjall 18.5% lower |
| Median host writes | 1.759 GB | 5.281 GB | Fjall 3.003x fewer |
| Median RSS delta | 60.6 MiB | 60.6 MiB | equal |
| Stored bytes | 277–284 MB | 101.1 MB | Fjall median 2.79x |
| Ending write buffer | 23–41 MiB | — | — |
| Ending L0 tables | 13–16 | — | — |

Every run reopened with exact per-keyspace row counts.

One clean-tree paired ten-cycle run (1,000,000 logical mutations) is a longer
directional check, not a substitute for repetitions:

| Metric | Fjall, 2 workers | Redb | Difference |
| --- | ---: | ---: | ---: |
| Throughput | 26,966/s | 16,095/s | Fjall 1.675x |
| Commit p50 | 111.6 ms | 163.4 ms | Fjall 31.7% lower |
| Commit p95 | 188.5 ms | 299.7 ms | Fjall 37.1% lower |
| Commit p99 | 276.4 ms | 574.4 ms | Fjall 51.9% lower |
| Host writes | 3.720 GB | 10.984 GB | Fjall 2.952x fewer |
| RSS delta | 60.6 MiB | 60.6 MiB | equal |
| Stored bytes | 354.7 MB | 101.1 MB | Fjall 3.51x |
| Ending write buffer | 31.6 MiB | — | — |
| Ending L0 tables | 19 | — | — |

The longer Fjall run stayed at the requested buffer scale and preserved the
write/throughput result, but its L0 and stored-byte growth still require a
longer steady-state/idle-drain falsifier. This evidence does not claim that all
compaction debt is retired.

## Other observed tradeoffs

- The insert-only default Fjall profile gained throughput partly by retaining
  about 91 MiB of backend working state. Tight 1-worker bounds reduced that
  delta by about 80% but cost roughly 23% throughput and caused flush backlog
  under repeated updates.
- Forcing all memtables to disk and major-compacting every keyspace inside one
  100k timing window reversed the throughput win, while still writing about
  2.45x less than Redb. That is a deliberately harsh deferred-maintenance
  ceiling, not a production compaction schedule.
- Fjall defaults to up to 4 background workers. NMP must explicitly budget the
  2 workers used here; inheriting the default would violate its bounded thread
  accounting.
- Fjall 3.1.6 declares Rust 1.90 minimum. Filesystem and background-thread use
  mean browser/WASM support is not established by the crate being pure Rust.

## Decision

Keep Fjall as the leading physical-store candidate. Under the qualified
2-worker profile it shows a repeated 35.6% median sustained throughput gain,
3.0x fewer host writes, equal measured RSS growth, and better p95/p99 commit
latency. The cost is 2 maintenance threads, slightly worse median commit
latency in the repeated run, and roughly 2.8x more live stored bytes.

The migration gate is now architectural: implement the governed `EventStore`
semantics once above a physical transaction seam, then rerun crash, query,
sustained-latency, RSS, idle-drain, and platform falsifiers on the real path.
A second Fjall-specific copy of Redb policy is not acceptable evidence.
