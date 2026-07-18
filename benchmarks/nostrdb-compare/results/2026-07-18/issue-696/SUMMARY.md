# Issue #696 — immutable canonical-event runs

## Decision

Do not promote immutable canonical-event runs into the governed store.

The candidate passed correctness, write, memory, disk, reopen, and query gates,
but missed the load-bearing performance gate. Across 10 alternating
fresh-process pairs it improved paired-median throughput by only **5.1%** and
reduced paired-median maintenance-inclusive wall by **4.8%**, against the
predeclared **20%** store-wall requirement.

The benchmark adapter is therefore reverted. The raw evidence remains because
it identifies a useful representation effect without justifying the production
complexity.

## Constant comparison

- Corpus: 100,000 representative events, BLAKE3
  `5eb48a3d4e4d051619c9f6656eed697dd1c1bf8eb210de5f9211ec7c0178ad36`.
- 10 alternating fresh-process pairs.
- Existing transaction batch: 4,096 events.
- Control: one Redb `EVENTS` value per event plus current packed postings.
- Candidate: one immutable canonical run per transaction, an ordered range
  catalog, unchanged `EVENT_IDS`, observations, relay rows, packed postings,
  durability, deletion overlays, and compaction geometry.
- Source commit: `beaa5d65f2f7f7179111fef33b9b596d8d6c8e0e`.
- Every child reported a clean tree and exact reopen.

## Results

| Metric | Packed Redb | Canonical runs | Paired-median change |
|---|---:|---:|---:|
| Throughput | 116,442 events/s | 122,318 events/s | **+5.1%** |
| Ingest wall | 859.1 ms | 817.6 ms | **-4.8%** |
| Redb commit | 430.2 ms | 439.4 ms | **-1.5% paired** |
| Process writes | 193.6 MB | 172.6 MB | **-10.9%** |
| Peak RSS | 202.5 MB | 208.4 MB | **+2.9%** |
| Allocated database bytes | 185.9 MB | 118.5 MB | **-36.3%** |
| Reopen plus full validation | 376.5 ms | 360.5 ms | **-4.7%** |

The candidate reduced canonical insertion from a median 85.7 ms to 13.6 ms of
run building plus 24.2 ms of insertion, about **56% less canonical work**. That
did not translate through Redb commit: the paired-median commit improvement was
only 1.5%, and absolute medians were effectively flat inside the observed
variance. This is why the end-to-end gain stopped near 5%.

All measured selective-query p95 results improved:

| Query | Paired-median p95 change |
|---|---:|
| Global newest 200 | -33.4% |
| First author | -4.8% |
| Busiest kind 200 | -36.8% |
| Busiest tag 200 | -29.4% |

The candidate retained immutable bytes for deleted events and removed only the
ID/observation claims plus packed death overlays. The metrics expose all 25
canonical runs and their retained bytes after deletion; no physical-GC saving
is claimed.

## Correctness evidence

- Checked run header, version, reserved bytes, key range, offset directory,
  nested event values, truncation, and trailing-byte refusal.
- Exact first, last, absent, raw-ID, and event-key lookup.
- Full reopen validation of every run, catalog entry, live ID, event ID, and
  packed membership.
- Committed run/catalog/ID/posting artifacts survived abrupt process exit;
  staged artifacts in the interrupted transaction were absent.
- All 20 matrix children reopened with the exact expected live canonical IDs
  and packed memberships.

## Interpretation

Packing canonical values moves NMP in the right direction for write endurance
and disk footprint, and it makes canonical lookup faster when query-local keys
are grouped by run. It is not the massive remaining constraint. Removing
100,000 canonical B-tree inserts saved local mutation time, but it did not
materially remove the Redb commit owner. A production format, retained-death
GC, migration, mutable provenance, and recovery machinery are not justified by
a 5% representative-ingest gain.

