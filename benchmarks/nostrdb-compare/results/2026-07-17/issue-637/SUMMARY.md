# Issue #637 — Redb index-tree fan-out result

This checkpoint asks whether Redb's dominant durable-commit cost comes from
mutating five separate ordered/tag index trees. The candidate stores the same
index records and values in one byte-keyed tree with a collision-free namespace
prefix. Canonical rows, event IDs, provenance, relay dictionaries, cardinality,
transaction size, and durability remain unchanged.

## Result

Eleven clean-tree paired repetitions processed the same representative
100k-event corpus in alternating order with 4,096-event transactions. Every
run reopened with the exact expected row count for each logical keyspace.

| Median metric | Separate index trees | Unified index tree | Difference |
| --- | ---: | ---: | ---: |
| Throughput | 75,207 events/s | 65,133 events/s | Unified 13.4% slower |
| Wall time | 1.330 s | 1.535 s | Unified 15.5% longer |
| Commit time | 0.717 s | 0.832 s | Unified 15.9% longer |
| Host writes | 342.6 MB | 363.3 MB | Unified 6.1% higher |
| Stored bytes | 73.1 MB | 73.6 MB | Unified 0.7% higher |
| Peak RSS | 426.6 MB | 426.5 MB | effectively equal |

Redb timing was variable across the matrix, so the load-bearing comparison is
the paired repetition ratio. Its median is also a 13.4% throughput regression.
The unified tree won only one pair materially and lost nine; the result is not
an artifact of comparing two independently noisy backend medians.

Unified logical validation takes longer because it scans the combined tree to
recover per-namespace counts, while separate Redb tables expose constant-time
length metadata. That validation cost is outside ingest wall and commit time,
so it does not cause the throughput result.

## Correctness falsifiers

- Namespace prefixes preserve every prepared key without collision.
- Reopen reconstructs and checks all twelve logical keyspace cardinalities.
- All twenty-two matrix runs pass exact reopen.
- An abrupt-exit test proves a committed unified-index row survives and a
  staged uncommitted row does not.

## Decision

Close the unified-tree hypothesis negative. Reducing physical index-tree count
does not remove Redb commit amplification; it makes commit slower and writes
more bytes for equivalent work. Do not propose a production schema migration
or pay selective-query locality risk for this layout.

The dominant cost follows the dirty page/key workload more than the number of
index roots. Further storage work should change index write volume or durable
page behavior materially, rather than repack the same records into fewer trees.
