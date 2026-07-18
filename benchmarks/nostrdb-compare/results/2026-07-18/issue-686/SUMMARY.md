# Active relay-ingest completion critical path

Issue #686 profiled the exact interval governed by the representative-ingest
completion clock. The profile covers every active thread, beginning immediately
before ingest starts and ending when the accepted projection first becomes
complete. This removes startup, quiet-proof, EOSE, and shutdown work from the
critical-path attribution.

## Pacing owner

The serial engine reducer is the completion pacer.

The 1,168.8 ms active interval contained 6,734 userspace samples. Signature
verification workers accounted for 4,797 samples (71.2%), but that work ran
across 8 parallel workers; the busiest worker had 745 samples. The single
engine thread had 883 samples (13.1%) and spent 1,102.3 ms processing batches.
The bridge waited 1,106.2 ms for applied acknowledgements, so engine processing
occupied 94.3% of the governed completion interval.

| Active thread group | Samples | Share |
|---|---:|---:|
| 8 signature-verification workers | 4,797 | 71.2% |
| Serial engine reducer | 883 | 13.1% |
| Transport reader and parser | 839 | 12.5% |
| Probe, relay, and engine-pool support | 215 | 3.2% |

Parallel verifier CPU is substantial compute cost, but it is not the current
latency owner. The engine thread remains active after individual verifier lanes
finish and controls when applied acknowledgements reach the benchmark.

## Serial engine split

Within the engine's governed work, resolver/store mutation and committed query
projection remain serialized:

| Measured phase | Time |
|---|---:|
| Resolver boundary CPU | 604.7 ms |
| Resolver-owned CPU | 560.9 ms |
| MemoryStore insertion | 271.4 ms |
| Committed mutation application CPU | 252.1 ms |
| Bounded-history projection wall time | 249.5 ms |

The engine profile had 867 samples in
`reduce_and_dispatch_relay_frames`, 785 in `ingest_relay_observations`, 682 in
the resolver, 239 in MemoryStore insertion, and 222 in
`apply_committed_mutation_with`. It contained no scalar `on_relay_frame` or
`refresh_all_histories` sample, confirming that #684 removed EOSE work from
this interval.

This is why a physical database swap cannot by itself close the measured
MemoryStore ceiling: the exact no-persistence path is already paced by serial
resolver plus post-commit projection work.

## Rejected candidate

The one allowed candidate ranked borrowed rows from each committed batch and
truncated them to the bounded history window before cloning/materializing
projection state. Five fresh-process pairs alternated baseline/candidate order.

| Metric | MemoryStore median paired change | Redb median paired change |
|---|---:|---:|
| Complete relay-ingest throughput | **+3.7%** | **-1.7%** |
| Relay reducer CPU | **-8.5%** | **-9.6%** |
| Committed history projection | **-34.4%** | **-85.9%** |
| Peak ingest RSS | **-0.1%** | **+0.3%** |
| Allocated bytes / process writes | **-4.2%** | **-0.2%** |
| Committed projection event clones | **-62.4%** | **-94.8%** |

The candidate reliably removed history work, but MemoryStore throughput missed
the required 5% gate and Redb's median throughput regressed. It was therefore
reverted. No production optimization is retained by this issue.

## Consequence

The next credible large lever is architectural: overlap the next batch's
resolver/store mutation with the prior committed batch's ordered query
projection, while preserving bounded backpressure, deterministic projection
order, and applied-ack correctness. The measured serialized slices make the
expected ceiling material; another isolated store or projection micro-change
does not.
