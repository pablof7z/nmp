# Issue #655 — complete packed-postings format qualification

## Decision

Proceed to #650's governed production integration using **packed Redb**. Do not
migrate the event plane to Fjall on this evidence.

The clean paired median, including mandatory compaction, reduced store elapsed
time by `26.1%`. Applying that sustained result to #647's measured `53%` store
share projects a `13.8%` full-pipeline elapsed-time improvement. This clears
#655's `10%` gate with room for integration overhead, but it remains a
projection until #650 measures the complete governed production path.

Process writes fell `50.8%`, stored bytes fell `23.8%`, and peak RSS rose
`8.8%`. The RSS result remains inside the issue's `10%` limit. Fjall consumes
byte-identical encoded values but remains slower than packed Redb, uses more
RSS, and stores far more physical data.

## Qualified format

The reusable codec replaces one row per index membership with immutable runs:

- one fixed-width `(event_key, event_id)` dictionary per logical run;
- one segment per populated family/shard/run;
- exact prefix offsets and binary prefix lookup;
- fixed `12`-byte `(created_at, dictionary_ordinal)` postings;
- exact `created_at DESC, event_id ASC` ordering;
- binary `before` and `until` seeks without decoding preceding postings;
- ID-only projection through the run dictionary with no canonical event read;
- at most `8` immutable per-run death blocks, merged with a hard fan-in bound;
- non-overlapping versioned run metadata;
- strict corruption, truncation, trailing-byte, and missing-dictionary refusal.

The random-access posting entry makes a restart directory unnecessary, so the
matrix reports `0` restart/seek-directory bytes. Adversarial tests prove exact
same-second cursors visit no preceding rows.

Compaction now reads exact family/shard/run keys rather than scanning whole
segment tables, consumes only persisted dictionaries/segments/death blocks,
and atomically removes source artifacts while publishing output. A completely
dead cohort is valid: its source artifacts are removed and no replacement run
is emitted.

## Clean representative matrix

Five fresh-process repetitions alternated row Redb, packed Redb, and packed
Fjall over the same `100,000`-event corpus. Values are medians; the decision
uses paired per-repetition reductions because host I/O latency varied across
the run.

| metric | row Redb | packed Redb | packed Fjall |
|---|---:|---:|---:|
| foreground wall | 2,773.8 ms | **1,551.9 ms** | 2,271.2 ms |
| maintenance | — | **241.9 ms** | 309.1 ms |
| foreground + maintenance | 2,773.8 ms | **1,795.5 ms** | 2,580.3 ms |
| process writes | 375.7 MiB | **184.6 MiB** | 132.1 MiB |
| stored bytes | 77.2 MiB | **58.9 MiB** | 207.0 MiB |
| peak RSS | **180.0 MiB** | 195.8 MiB | 206.6 MiB |

The ratio of independent medians implies a larger gain, but the paired median
is the conservative decision statistic: `26.1%` sustained store-time
reduction, projecting to `13.8%` full-pipeline elapsed-time reduction.

## Format cost

Before compaction, the deterministic format totals were:

| section | rows | logical bytes |
|---|---:|---:|
| segments | 4,264 | 14,795,467 |
| run dictionaries | 25 | 4,000,300 |
| run metadata | 25 | 1,025 |
| death blocks | 25 | 12,795 |
| restart/seek directory | — | 0 |

The segments contain `206,939` prefix records and `428,220` postings. After
compaction, `705` segment rows, `4` dictionaries, and `4` run-metadata rows
remain active. Reopen recovered exactly `87,500` live canonical events and the
expected memberships after the deterministic deletion wave.

## Query evidence

Every packed query repetition matched the corpus oracle after deletion and
compaction. Packed Redb median p95 was:

| query | p95 |
|---|---:|
| newest 200 | 13.7 us |
| one-row author | 3.9 us |
| busiest kind, bounded 200 | 14.3 us |
| busiest tag, bounded 200 | 10.2 us |

The directly comparable production row measurements were slower for the
one-row author and bounded-kind paths. The packed path also projects IDs
without reading canonical values, preserving the existing large-event
falsifier.

## Consequence for #650, #627, and #612

The remaining dominant constraint is Redb copy-on-write work from one physical
row per index membership. The complete packed format attacks that constraint
and retains roughly half the process writes after adding dictionaries, exact
cursors, deaths, metadata, and persisted-data compaction.

#650 should now integrate this format behind the existing governed event-store
transaction boundary and measure the actual full pipeline. The publishing
control-plane authority split is independently sensible, but it is not the
measured representative-ingest bottleneck and should not be mixed into this
performance change.
