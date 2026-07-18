# Issue #658 — packed commit attribution and LMDB ceiling

## Decision

Close the LMDB production-migration branch negative and move #612 to the
parse/verification/materialization constraint.

Equivalent synchronous LMDB is genuinely faster: across 20 fresh-process
alternating pairs it reduced maintenance-inclusive governed-store wall by
21.3%, which projects to 11.5% more throughput in the merged production relay
pipeline. It nevertheless fails the issue gate because peak RSS rose 48.3%.
The adapter is therefore retained only as benchmark evidence; it is not a new
production backend, and the one-million run was correctly skipped.

The Redb non-durable diagnostic independently proves that durability and
durable free-page/header work are material. After paying one explicit final
Immediate checkpoint, it reduced governed-store time 23.3% and complete
ingest-plus-shutdown wall 10.8%. That mode can lose every foreground commit on
crash and is not shippable. Even this unsafe ceiling does not change the larger
conclusion: storage work alone cannot carry the 150,000 events/s target.

## Redb durability attribution

Ten production relay-ingest pairs alternated current Immediate commits and
benchmark-only `Durability::None` foreground commits followed by one timed
Immediate checkpoint. Every run crossed websocket transport, JSON parsing,
signature verification, resolver policy, governed storage, bounded live-query
delivery, shutdown, and reopen verification over 100,000 events.

| metric | paired median result |
|---|---:|
| foreground relay throughput | **16.5% higher** |
| foreground ingest wall | **14.1% lower** |
| ingest + shutdown | **10.8% lower** |
| governed store foreground | **29.7% lower** |
| governed store + checkpoint | **23.3% lower** |
| Redb commit foreground | **78.2% lower** |
| Redb commit + checkpoint | **61.0% lower** |
| packed publication + compaction | 3.9% higher |
| process writes, checkpoint included | **40.1% lower** |
| peak RSS | 1.4% higher |
| physical database size | unchanged |
| reopen and verify | 0.6% higher |

The old attribution called packed publication, synchronous compaction, and the
Redb transaction commit one `commit` bucket. The benchmark seam now records
canonical pending flush, packed-postings flush, actual Redb commit, and the
deferred durability checkpoint separately. Packed work was essentially
unchanged between modes; the reduction belongs to the Redb durability path.

Redb 4.1's source makes the mechanism concrete. Both durability modes finalize
dirty tree checksums and flush the write buffer to the file. Immediate commit
additionally processes durable freed pages, updates the system tree and
allocator state, writes/switches the database header, and calls `fdatasync`.
The non-durable path leaves those commits pending until an Immediate commit.
A syscall-count diagnostic observed 46 `fdatasync` calls for Immediate versus
17 for non-durable plus checkpoint; the 29-call delta tracks the foreground
commit cohort. Redb does not expose per-transaction dirty/allocated/freed page
counts, so process-write bytes, allocated blocks, source-path attribution, and
the durability A/B are the available falsifiers rather than invented page
counts.

## Exact synchronous LMDB ceiling

The native-only adapter uses heed 0.22.1 / LMDB in one environment with 26
named databases and default synchronous flags. It reuses the production
governed insert state machine, portable canonical codecs, sampled-cardinality
policy, packed dictionaries and segments, death blocks, and the exact 8/6
compaction fan-in. Every batch commits canonical and packed effects in one LMDB
write transaction.

Twenty pairs alternated governed packed Redb and governed packed LMDB with the
same 4,096-event batches and 100,000-event corpus. All 40 children reopened
with exact canonical IDs. The LMDB verifier also decoded every dictionary,
segment, run catalog entry, and death block, reconstructed all live
memberships, and compared them with the canonical rows.

| metric | governed Redb median | governed LMDB median | result |
|---|---:|---:|---:|
| store wall | 1,528.6 ms | 1,188.7 ms | **21.3% lower paired median** |
| store throughput | 65,426 events/s | 84,124 events/s | **27.1% higher paired median** |
| governed apply | 632.5 ms | 358.6 ms | 43.3% lower |
| canonical pending flush | 16.3 ms | 6.1 ms | 62.6% lower |
| packed publication + compaction | 247.4 ms | 219.5 ms | 11.3% lower |
| synchronous commit | 573.9 ms | 602.5 ms | 5.0% higher |
| process writes | 237.6 MiB | 218.9 MiB | 7.9% lower |
| peak RSS | 199.8 MiB | 296.3 MiB | **48.3% higher — gate failure** |
| allocated database blocks | 144.2 MiB | 119.9 MiB | 16.9% lower |
| logical file length | 257.0 MiB | 119.9 MiB | 53.4% lower |
| open + canonical-ID enumeration | 235.7 ms | 18.3 ms | 92.3% lower |

The paired store-wall reduction applied to #650's measured 1,680.6 ms store
share of a 3,459.6 ms pipeline projects 3,101.6 ms total wall, or 11.5% higher
throughput. It clears the speed thresholds but not the memory threshold. LMDB
won throughput in 18 of 20 pairs; host I/O variance remained large, so the
paired median is the decision statistic rather than a cherry-picked run.

The exact 100,000-event LMDB work published 25 runs and performed 3 synchronous
compactions over 24 input runs, rewriting 98,304 events. This matches the
production compaction geometry rather than benchmarking an easier index set.

## Why commit grows at one million

At 100,000 events the 25 batches trigger 3 level-0 compactions and almost the
whole corpus is rewritten once. At one million, #650's 247 governed batches
trigger 30 level-0 and 5 level-1 compactions; almost the whole corpus is
rewritten through 2 levels. The larger canonical tree, repeated packed cohort
rewrites, copy-on-write replacement, and the durable freed-page/system-tree
path all dirty more data and retain more allocator history. This is consistent
with the observed Redb commit growth from about 0.9 seconds at 100,000 to 27.3
seconds at one million and process writes growing from about 0.24 GiB to 4.86
GiB. It is not outbox work: representative remote ingest creates no publishing
control rows.

## Correctness and stop boundary

The LMDB ceiling is exact for the write representation and governed mutation
policy, not a hidden production backend. A focused test covers replacement,
kind-5 deletion, small-batch compaction, exact canonical reopen, and exact
packed reconstruction. Clean-process evidence covers 40 exact reopens.

No LMDB query engine, selective-query p95 matrix, or process-death crash matrix
was built after the peak-RSS gate failed. Those are mandatory before any future
production integration, not costs this negative ceiling is allowed to hide.
The one-million run was also skipped exactly as #658 requires after a material
100,000-event regression.

## Portability and Fjall

- Linux: heed bundles LMDB's C implementation through `lmdb-master-sys`; the
  checked comparator builds and runs on the measured x86-64 host.
- Apple: a product backend would add a C/static-library cross-build and signing
  integration for each Apple target.
- Android: it would require NDK builds per ABI plus explicit validation on
  modern 16 KiB-page devices.
- Browser/WASM: heed/LMDB has no browser storage path. NMP's browser backend is
  already deferred; Redb was not selected because browser persistence was
  solved, and LMDB would not solve it either.

Fjall was not discarded because write reduction is uninteresting. The existing
production-format evidence already measured its tradeoff: packed Fjall wrote
less, but maintenance-inclusive wall was about 44% slower than packed Redb,
used more RSS, and stored roughly 3.5 times the physical bytes. Repeating the
same multi-keyspace shape would add no new information. A one-keyspace,
table-prefixed Fjall layout remains a distinct future hypothesis, but it is not
a no-tradeoff migration and is not the dominant next experiment.

## Authority boundary

The architecture relaxation raised in #627 is accepted and now explicit in
`docs/VISION.md`: durable `Accepted` is a logical crash-consistent boundary,
not a mandate for one physical database transaction. A split control store may
commit the publishing obligation first and project it idempotently before
queries/transport resume; event-local rows and indexes remain atomic inside the
event store. This does not accelerate representative remote ingest, so it is
kept separate from the performance decision.

## Consequence for #612

Do not migrate the production event plane in #658. Keep governed packed Redb,
retain the LMDB adapter as a reproducible native ceiling, and attack the work
outside storage next: JSON parsing, signature verification, event
materialization, and relay-to-resolver handoff. Even deleting all store time
from #650's 3.46-second pipeline would only reach roughly 56,000 events/s, so
the 150,000 events/s epic target necessarily requires those layers to change.
