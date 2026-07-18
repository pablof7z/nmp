# Issue #691 — one-keyspace Fjall packed qualification

## Decision

Reject Fjall for the current packed representation. Do not build a governed
adapter or a million-event follow-up.

Consolidating the logical tables into one prefixed Fjall keyspace does remove a
large part of the multi-keyspace wall penalty: paired medians improve foreground
ingest by `28.5%` and maintenance-inclusive wall by `12.7%` versus the existing
Fjall layout. That validates the physical-partition-overhead hypothesis.

It does not make Fjall competitive with packed Redb. Against the deciding Redb
control, the one-keyspace candidate is `49.3%` slower during foreground ingest
and `54.1%` slower after deletion and mandatory compaction. It also reverses the
interesting Fjall write reduction: total measured process writes are `18.6%`
higher than Redb and `90.2%` higher than multi-keyspace Fjall.

The candidate therefore fails the wall, projected-pipeline, writes, stored-byte,
and query gates. Exact reopen and every query oracle passed, and peak RSS stayed
inside the limit, but those successes do not rescue it.

## Clean representative matrix

Ten fresh-process triplets used the same `100,000`-event corpus and alternated
the outer layout order. Values below are independent medians for scale; the
decision percentages above use paired within-repetition changes.

| metric | packed Redb | multi-keyspace Fjall | one-keyspace Fjall |
|---|---:|---:|---:|
| foreground wall | **944.9 ms** | 2,072.7 ms | 1,471.9 ms |
| maintenance-inclusive wall | **1,348.4 ms** | 2,451.8 ms | 2,163.2 ms |
| mandatory compaction | **230.2 ms** | 337.1 ms | 612.0 ms |
| total process writes | 262.7 MiB | **163.8 MiB** | 311.5 MiB |
| backend-reported stored bytes | **58.9 MiB** | 207.0 MiB | 141.6 MiB |
| peak RSS | **191.2 MiB** | 205.8 MiB | 208.0 MiB |

One-keyspace Fjall peak RSS is `8.8%` above Redb, within the `10%` gate.
Backend-reported stored bytes are `140.6%` above Redb. Sparse filesystem block
allocation reports a different direction, so it is preserved in the raw file
rather than substituted for the benchmark's existing stored-byte measure.

## Query result

Every run returned the exact oracle rows after deletion and compaction. Median
p95 latency nevertheless misses the `10%` gate by orders of magnitude:

| query | packed Redb | one-keyspace Fjall |
|---|---:|---:|
| newest 200 | **13.4 us** | 402.7 us |
| one-row author | **3.6 us** | 235.2 us |
| busiest kind, bounded 200 | **13.0 us** | 295.3 us |
| busiest tag, bounded 200 | **9.1 us** | 243.5 us |

The paired p95 regressions range from roughly `22x` to `63x`; this is not a
noise-level tradeoff.

## Consequence

The earlier Fjall write reduction was real, but it came from a physical layout
whose wall, memory, disk, and query costs already failed. Collapsing partitions
improves its wall time while destroying that write advantage and still leaves
it far behind Redb. There is no tradeoff-free Fjall migration in the tested
packed design.

Future Fjall work needs a materially different representation or query
strategy, not another partition-count tuning pass. The current dominant
performance work should remain on reducing the governed Redb path and the
non-storage pipeline ceiling identified by #658 and #688.
