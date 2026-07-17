# Issue #648 — packed ordered-postings result

## Decision

Advance the packed ordered-postings design on **Redb** to a governed production
integration issue. Do not migrate the event plane to Fjall on this evidence.

The strict `2x` store-ceiling gate did not pass: packed Redb was `1.448x`
faster than the merged row layout in the paired median. The quantified tradeoff
is nevertheless strong enough to proceed: foreground ingest time fell `30.9%`,
process writes fell `53.3%`, stored bytes fell `32.1%`, and peak RSS rose `7.4%`.
The user explicitly accepted advancing a material gain even when it does not
independently close the full performance program.

This is a physical-layout selection, not a production performance claim. Using
the #647 median full-pipeline decomposition as a first-order projection, applying
the observed store-time ratio would improve representative production ingest by
about `20%`. Only the later end-to-end port can confirm that estimate.

## Format proved

The prototype keeps canonical event, event-ID, and provenance rows in their
existing representation and replaces per-membership ordered-index rows with
immutable segments:

- four families: global, author, kind, and tag;
- `64` keyed BLAKE3 shards per family;
- exact prefix bytes stored once per shard/run;
- postings ordered exactly by `created_at DESC, event_id ASC`;
- timestamp deltas and zigzag event-key deltas;
- no repeated event ID in postings; canonical IDs are loaded only to resolve
  equal-timestamp ties across runs;
- size-tiered compaction with fan-in `8`;
- delta-coded dead-event-key blocks for deletion, replacement, expiry, and GC
  overlays.

Redb and Fjall consume byte-identical segment and dead-key values.

## Representative 100k matrix

Five clean fresh-process repetitions alternated row Redb, packed Redb, and
packed Fjall over the same representative corpus. Values below are medians;
the speed decision uses the paired median.

| metric | row Redb | packed Redb | packed Fjall |
|---|---:|---:|---:|
| foreground events/s | 67,618 | **97,907** | 51,929 |
| foreground wall time | 1,478.9 ms | **1,021.4 ms** | 1,925.7 ms |
| commit time | 773.7 ms | **635.4 ms** | 1,628.4 ms |
| process writes | 375.7 MiB | **175.6 MiB** | 123.4 MiB |
| peak RSS | **179.9 MiB** | 193.2 MiB | 204.9 MiB |
| stored bytes | 77.2 MiB | **52.5 MiB** | 197.1 MiB |
| paired throughput ratio | 1.000x | **1.448x** | 0.791x |

Fjall's lower foreground writes do not offset its `21%` throughput regression,
larger RSS, `2.6x` stored size, and slower selective queries. It remains a
valid engine, but it is not the winner for this workload and format.

## Queries, deletion, and maintenance

Every query repetition matched the corpus oracle after the deletion overlay and
compaction. Packed Redb median p95 was:

| query | p95 |
|---|---:|
| newest 200 | 54.6 us |
| one-row author | 5.9 us |
| busiest kind, bounded 200 | 54.4 us |
| busiest tag, bounded 200 | 58.6 us |

The directly comparable one-row author path remains around the current row
layout's p95. The bounded kind/tag paths are faster than the existing production
bounded measurements, although the prototype does not yet exercise every
compound production filter shape.

Each run then removed a deterministic `12.5%` of canonical events and recorded
one delta-coded dead-key block per source run. At 100k this was `25` blocks and
only `12.5 KiB` of logical overlay data. Packed Redb's median deletion phase was
`260.8 ms` and `55.3 MiB` of process writes. A replacement is the same old-key
death plus the already-measured ordinary append for the new incarnation.

Quiescent fan-in compaction took `173.1 ms` and `14.2 MiB` of process writes on
packed Redb. It reduced active segment rows from `4,264` to `705`; active segment
values occupied `8.8 MiB`. Compaction atomically removes source segments and
their dead-key blocks while publishing equivalent live postings.

## Crash and scale proofs

The forced-exit test commits a canonical row and segment together, stages a
second generation, and calls `_exit`. Reopen observes the committed pair and
neither staged row. Segment decoding, dead-key decoding, exact equal-timestamp
ordering, and full-ID cross-run tie resolution are unit-tested.

The one-million representative run consumed the same format, applied `125,000`
deterministic deletions, compacted, and reopened the exact `875,000` live
canonical rows with exact memberships and query results. Foreground throughput
was 63,936 events/s. This is a scale/recovery proof, not a repeated performance
matrix; its high RSS includes retaining the full decoded one-million-event
benchmark oracle in memory.

## Consequence for #627 and #612

Packed representation is the selected remaining storage lever. It materially
reduces write amplification but cannot supply #627's or #612's multiplier by
itself. The next issue should port this design behind the existing event-store
authority boundary and measure the complete resolver/engine/live-query path.
The cross-store atomicity relaxation already recorded in #627 permits that
event-plane change without forcing the low-volume publishing control plane into
the same physical engine.

