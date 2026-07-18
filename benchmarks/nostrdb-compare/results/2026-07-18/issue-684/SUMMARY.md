# EOSE evidence-refresh profile and retained candidate

Issue #684 symbolized the unresolved relay-planning residual left by #676.
The profile did not find session planning. It found a coverage-only EOSE
calling the full history projection oracle after every representative ingest.

## Profiled owner

The baseline engine thread contained 1,583 sampled stacks. The final scalar
`on_relay_frame` EOSE chain appeared in 317 of them (20.0%): every one passed
through `refresh_all_histories` and `refresh_history`, and 270 reached
`MemoryStore::query_newest`.

That work is unnecessary for a coverage-only mutation. EOSE can advance
`AcquisitionEvidence`, but it cannot add, remove, replace, or change the
provenance of a canonical row. The old path nevertheless queried the full
100,000-event store to reproduce a bounded 200-row history projection.

The committed profile evidence includes the exact perf headers and the raw,
symbolized engine-thread sample streams. The candidate profile contains no
`on_relay_frame` sample: the identified EOSE query chain is gone.

## Retained candidate

Coverage-only completion now has an evidence-only projection path for live and
history observations. It recomputes scoped evidence, emits an empty row delta
when that evidence changes, and retains the already-authoritative row set.
NEG coverage completion uses the same path.

The fail-safe rule is structural: if a prior store failure marked either
projection incomplete, evidence-only refresh falls back to the existing full
row oracle before anything is emitted. Focused falsifiers cover live EOSE,
history EOSE, zero event-index reads, unchanged remembered rows, and both
incomplete-projection fallbacks.

## Paired result

Five fresh-process pairs alternated baseline/candidate order. The decision uses
the median of the five within-pair changes.

| Metric | Median paired change |
|---|---:|
| Complete relay-ingest throughput | **-0.4%** |
| Completion time | **+0.4%** |
| Relay reducer CPU | **-30.4%** |
| Peak ingest RSS | **-37.0%** |
| Allocated bytes | **-6.8%** |

Complete throughput is unchanged within run noise, so this result contributes
no claimed throughput gain toward the 150,000 frames/s thesis gate. The
candidate is retained because it removes a symbolized full-store scan, cuts a
large and repeatable CPU/memory cost, and preserves the full recovery oracle.
This is headroom and a scaling correction, not the missing throughput lever.

## Consequence

The unexplained relay-planning residual from #676 is now explained and
removed. The remaining headline throughput constraint is concurrent pipeline
work during event ingestion—verification, parsing, resolver/store mutation,
and committed projection—not EOSE/session planning and not the physical
database choice by itself.
