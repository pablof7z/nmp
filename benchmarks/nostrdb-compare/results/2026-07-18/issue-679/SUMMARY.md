# Resolver/store/projection CPU attribution

Issue #679 followed the shifted-bottleneck result from #675 and #676. The
question was not whether another plausible optimization existed, but which
non-overlapping owner consumed the remaining complete-ingest CPU and whether
one production-shaped change improved the real pipeline.

## Reconciled owner

The schema-v20 counters reconcile the engine's complete ingest observation
call to its resolver, prelude, post-store, and committed-projection children.
The representative MemoryStore baseline put the largest credible semantic
owner inside `EventStore::insert_batch`: about 290 ms median CPU before the
more detailed instrumentation.

The corrected schema-v21 3-process median split MemoryStore insertion as:

| Phase | Median | Share of insertion |
|---|---:|---:|
| Complete MemoryStore insertion | 266.7 ms | 100% |
| Secondary query indexes | 137.8 ms | 51.7% |
| Event/provenance construction | 40.0 ms | 15.0% |
| Canonical `by_id` insertion | 33.2 ms | 12.4% |
| Expiration index | 2.7 ms | 1.0% |
| Remaining governed semantics | 53.0 ms | 19.9% |

Secondary-index maintenance was therefore the measured owner, not a guess
based on aggregate store time.

## Retained candidate

`MemoryStore` previously copied each 32-byte event id into every ordered
secondary index and copied each 32-byte author into both author indexes. The
candidate keeps public/canonical identities unchanged, but assigns private,
monotonic, never-reused `u64` event and author keys to repeated index entries.
Bidirectional maps preserve exact lookup, author keys are reference-counted,
and the existing consistency falsifier now checks both dictionaries as well as
every index.

This is the same identity-compaction principle already used by the governed
Redb event plane. It does not truncate hashes and therefore introduces no
collision semantics.

## Paired result

Five fresh-process pairs alternated baseline/candidate order. The decision uses
the median of the 5 within-pair changes, which is less sensitive to machine
drift than dividing the two independent medians.

| Metric | Median paired change |
|---|---:|
| Complete relay-ingest throughput | **+2.5%** |
| Semantic store CPU | **-11.8%** |
| Secondary-index CPU | **-22.9%** |
| Peak ingest RSS | **-1.0%** |
| Allocated bytes | **+3.2%** |

Every pair reduced store and index CPU; 4 of 5 improved complete-pipeline
throughput. The extra dictionaries increase allocation traffic, but that did
not translate into higher peak memory. The CPU, throughput, and RSS directions
justify retaining the candidate; allocation traffic remains the explicit
tradeoff.

The event-key-only intermediate reduced memory but did not reliably reduce
index CPU. Author compaction is the load-bearing part because author is the
leading comparison field in 2 ordered indexes; the representative corpus also
has high author cardinality.

## Gate consequence

This is a real gain, but not the missing order-of-magnitude lever. Applied to
#675's favorable combined ceiling, a 2.5% improvement projects roughly
138,500 frames/s, still below the 150,000 frames/s thesis gate. The result
removes a measured store/index slice and leaves relay planning plus committed
projection as the larger remaining complete-pipeline domains.
