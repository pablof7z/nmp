# Bounded commit/projection pipeline result

Issue #688 is a measured negative. The candidate correctly admitted a bounded
second EVENT batch and began its store commit while the prior batch's
store-read-free history projection remained outstanding, but the complete
production gain was too small to retain the added concurrency.

## Decision matrix

Five fresh-process pairs alternated baseline/candidate order on the exact
100,000-event representative corpus.

| Metric | MemoryStore median paired change | Durable Redb median paired change |
|---|---:|---:|
| Complete throughput | **+1.1%** | **+2.3%** |
| Peak ingest RSS | +0.4% | +2.8% |
| Allocated bytes | -3.7% | -10.4% |
| Process writes | not applicable | -0.3% |
| Bridge applied-wait time | -88.3% | -4.0% |

The throughput gate required +10% MemoryStore and +5% Redb. A preliminary
uninstrumented set appeared to show +11.0% Redb and prompted a public proposal
to amend the gate around the production backend. The fresh final set above did
not reproduce that result. The production code was therefore reverted rather
than retaining a roughly 1,100-line concurrency change for a 2.3% paired gain.

## Mechanism proof

The final candidate used a depth-two bridge window and a single bounded
projection worker. Every candidate run reported a maximum of exactly two
outstanding bridge batches and zero worker failures.

On durable Redb, the next commit began while the prior projection was
outstanding for 26–27 of the 28–29 resolver batches in every run. This proves
that the low gain is not a failed-overlap artifact. The median candidate spent
about 1.78 seconds in the governed Redb transaction boundary but only about
0.21 seconds in the projection worker across the complete run. The physical
store boundary remains the larger production constraint.

MemoryStore reached the depth-two window in every run, but its faster store
made producer arrival and worker completion race; the overlap count varied
from 38 to 114 batches. Removing most applied-wait time still left complete
throughput effectively flat, confirming that this dependency is not the next
MemoryStore-scale lever.

## Correctness result

All 20 decision runs observed exactly 100,000 relay frames and ended with the
expected 200-row bounded window. Durable Redb runs completed exact reopen
verification. Focused bridge, worker-bound, shutdown-lifecycle, and end-to-end
MemoryStore/Redb smoke tests passed before the candidate was reverted.

## Consequence

Do not reintroduce the pipeline without a materially larger projection slice
or a different workload. The next store experiment should first split packed
publication/compaction time from the actual Redb commit rather than treating
the existing combined commit bucket as one physical backend cost.
