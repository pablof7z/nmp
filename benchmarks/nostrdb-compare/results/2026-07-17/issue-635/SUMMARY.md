# Issue #635 — governed Fjall ingest result

This checkpoint tests the result from issue #629 on NMP's real governed ingest
policy rather than the prepared 12-keyspace physical workload. The benchmark
uses the shared `GovernedIngestTxn` seam from issue #633, so Redb and Fjall run
the same canonical-row, provenance, replacement, deletion, expiration,
tombstone, outbox, and derived-index policy before one atomic commit.

## Qualified comparison

Five clean-tree repetitions per backend processed the same representative
100k-event corpus in alternating order with 4,096-event transactions. Every
run reopened with exactly 100,000 canonical rows. The Fjall profile uses 2
maintenance workers, a 16 MiB cache, a requested 32 MiB write buffer, and 4 MiB
per-keyspace memtables.

| Median metric | Redb | Fjall | Difference |
| --- | ---: | ---: | ---: |
| Throughput | 50,575 events/s | 31,112 events/s | Fjall 38.5% slower |
| Wall time | 1.977 s | 3.214 s | Fjall 62.6% longer |
| Commit time | 1.035 s | 2.143 s | Fjall 2.07x |
| Host writes | 341.4 MB | 164.7 MB | Fjall 51.8% lower |
| Peak RSS | 184.6 MB | 241.8 MB | Fjall 31.0% higher |
| Stored bytes | 73.1 MB | 197.1 MB | Fjall 2.70x |

The paired repetition median agrees directionally: Fjall delivered 59.3% of
Redb throughput. One noisy Redb repetition nearly tied Fjall; it does not
reverse either the backend medians or four of the five paired outcomes.

## Attribution

Fjall's median commit time is 2.143 seconds, about 66.7% of its total wall
time. Governed policy application is 1.057 seconds. Within that policy work,
point reads consume 0.363 seconds and index mutations consume 0.315 seconds.
Encoding is negligible.

The dominant Fjall constraint is therefore durable commit under the real
multi-keyspace mutation shape, not event encoding or transaction point reads.
Increasing per-keyspace memtables improved exploratory throughput only by
buying substantially more RSS; those dirty-tree exploratory runs are not part
of the evidence matrix.

## Correctness falsifiers

The benchmark-only Fjall adapter has tests for duplicate provenance,
replaceable supersession, kind:5 deletion and tombstones, expiration,
single-letter tag indexes, cardinality, exact reopen, and abrupt process exit.
The abrupt-exit test verifies that a committed batch survives while an
uncommitted batch is discarded.

## Decision

Do not proceed to a full Fjall `EventStore` port from this physical layout and
bounded profile. The prepared-workload gain from issue #629 does not survive
the production governance path: the real comparison is 38.5% slower, exceeds
the epic's memory-regression gate, and uses 2.70x the stored bytes. The 51.8%
host-write reduction is real and remains useful evidence, but it is not free.

This is a negative gate for the tested design, not a claim that every possible
Fjall layout is slower. It intentionally stops before query, full outbox/lane,
million-row, mobile-packaging, and browser/WASM work because the governed hot
path already crosses the migration kill boundary. A future Fjall proposal must
first show a materially different physical layout that removes commit cost
without purchasing speed through RSS or disk growth.
