# Issue #650 — governed packed-postings production result

## Decision

Merge the governed packed Redb event plane.

Twenty fresh-process alternating pairs over the complete production relay
pipeline put the candidate at **12.8% higher paired-median throughput**. The
ratio of the independent medians is **10.4% higher**, and the candidate won
16 of 20 pairs. This clears #650's 10% production gate by both reported median
views after expanding the required five-pair matrix to resolve substantial
host commit variance.

The more stable physical results are stronger: process writes fell **41.8% in
all 20 pairs**, and peak RSS fell **26.7% in all 20 pairs**. The production
Redb file-allocation median did not change; the 23.8% logical/controlled-format
reduction measured by #655 is not visible through Redb's coarse physical file
allocation at this scale.

## Complete production pipeline

Every child crossed the real websocket, JSON parse, signature verification,
resolver, governed store, and bounded live-query path. Every child observed
and reopened exactly 100,000 events. Odd pairs ran baseline first; even pairs
ran candidate first.

| Metric | Baseline median | Candidate median | Paired median |
| --- | ---: | ---: | ---: |
| relay throughput | 26,174 events/s | 28,906 events/s | **12.8% higher** |
| full ingest wall | 3,820.6 ms | 3,459.6 ms | **11.3% lower** |
| store transaction | 2,012.9 ms | 1,680.6 ms | **19.4% lower** |
| store apply | 1,103.0 ms | 731.0 ms | **33.2% lower** |
| Redb commit | 873.9 ms | 901.0 ms | 2.3% lower |
| process writes | 404.4 MiB | 235.3 MiB | **41.8% lower** |
| peak RSS | 161.0 MiB | 117.8 MiB | **26.7% lower** |
| Redb file allocation | 257.0 MiB | 257.0 MiB | unchanged |
| first row | 13.1 ms | 12.4 ms | 1.0% higher |
| reopen and verify | 382.3 ms | 351.3 ms | 6.4% lower |

Commit latency was the source of most pair-to-pair variance: store apply was
faster in all 20 pairs, while commit won only 10. The paired throughput ratios
ranged from 0.815x to 1.707x. A fixed-seed 200,000-resample bootstrap puts the
paired-median ratio's 95% interval at 1.068x–1.164x, so the exact 10% threshold
remains inside the sampling uncertainty even though both observed median
estimators now pass it. This is a measured engineering gate, not a claim that
every individual run is faster.

## Query and first-row gate

Five alternating query pairs ran 50 iterations of complete kind, author,
author+kind, tag, two-tag, and 43-author queries plus bounded kind and tag-pair
queries. Every expected row count matched. The worst paired-median p95 ratio
was 1.057x for the complete one-row author query, inside the 10% regression
limit. Production first-row latency was effectively flat by paired median and
5.1% lower by the ratio of independent medians.

## One-million qualification

The candidate ingested and reopened exactly 1,000,000 events. Against the
exact merged baseline binary:

| Metric | Baseline | Candidate | Result |
| --- | ---: | ---: | ---: |
| relay throughput | 19,466 events/s | 19,815 events/s | 1.8% higher |
| store transaction | 42.86 s | 39.79 s | 7.2% lower |
| store apply | 18.44 s | 12.04 s | **34.7% lower** |
| Redb commit | 24.01 s | 27.33 s | 13.8% higher |
| process writes | 9.24 GiB | 4.86 GiB | **47.4% lower** |
| peak RSS | 181.6 MiB | 175.4 MiB | 3.4% lower |
| Redb file allocation | 2.01 GiB | 2.01 GiB | unchanged |
| first row | 13.6 ms | 11.6 ms | 14.7% lower |

The one-million run is a scale/recovery falsifier, not a repeated throughput
estimate. It shows that packed apply work continues to fall while Redb commit
becomes the dominant storage constraint.

## Maintenance and bounds

Packed compaction is synchronous inside the same governed Redb transaction as
the triggering ingest. The production timings therefore include all mandatory
amortized maintenance. There is no background compactor or unpublished work
queue, so quiescent maintenance debt and post-ingest maintenance time are both
zero when the ingest call returns.

The active-run invariant is mechanical: level 0 is compacted before it can
retain 8 runs, every higher level before it can retain 6, each compaction cohort
is bounded by that fan-in, and each run retains at most 8 immutable death
blocks before rewrite. Streaming compaction, a 12 MiB Redb cache, and bounded
large-run cohorts keep the exact one-million peak RSS below the prior baseline.

## Crash and recovery proof

Real child processes now abort before and after foreground segment publication,
catalog publication, governed ingest commit, dead-key publication, compaction
output, schema migration readiness/commit, and coverage-lowering GC. Reopen
proves either the complete old state or the complete new state; no canonical
row, packed artifact, coverage claim, or migration marker can escape alone.

The full `nmp-store` library suite passes with 120 tests and 1 ignored corpus
test. Ordinary `cargo check` is warning-clean, and all-target/all-feature clippy
passes with warnings denied.

## Consequence for #627 and #612

Packed postings are worth retaining. They remove row-per-membership index
mutation, cut real process writes by about 42%, and deliver about 13% observed
production throughput without weakening query, recovery, or memory behavior.

The next constraint is no longer index construction. It is Redb commit and
copy-on-write dirty-page work: at one million events, commit alone consumes
27.3 seconds. The next bounded experiment should attribute that cost and
compare the exact packed representation under synchronous LMDB and a Redb
non-durable diagnostic ceiling before considering any engine migration.
Fjall remains rejected by the production-equivalent evidence: its write
reduction came with worse throughput, RSS, and disk use.

