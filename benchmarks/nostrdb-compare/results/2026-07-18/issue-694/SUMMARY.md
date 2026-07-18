# Issue #694 — larger crash-atomic ingest cohorts

## Decision

Do not raise the production `4,096`-event cohort ceiling.

Larger cohorts attack the intended mechanism: they cut transaction count,
Redb commit time, and process writes. The practical `8,192` candidate reduces
the batch count by `40.7%`, Redb commit time by `19.9%`, and process writes by
`16.7%` on paired medians. Complete production throughput improves only `4.8%`,
below the issue's `10%` gate, while peak ingest RSS rises `17.9%`, beyond the
epic's `10%` allowance.

The `16,384` configuration is not a valid larger-cohort winner. It never forms
more than `8,704` events under the unchanged production queue and `200 us`
wait. Its paired throughput statistic is favorable amid severe Redb commit
variance, but peak RSS rises `24.9%`, and the configured cohort size is never
exercised. It fails regardless of which throughput estimator is preferred.

No production code changed. The issue stops before MemoryStore, duplicate,
one-million, or default-retention work, exactly as its stage-1 kill boundary
requires.

## Five-triplet production matrix

Values below are independent medians for scale. Decision percentages use the
median within-triplet change against `4,096`.

| metric | 4,096 control | 8,192 | 16,384 configured |
|---|---:|---:|---:|
| completion throughput | **40,593/s** | 41,331/s | 39,161/s |
| completion wall | **2,463.5 ms** | 2,419.5 ms | 2,553.5 ms |
| store batches | 28 | **16** | **16** |
| observed max batch | 4,096 | 8,192 | 8,704 |
| store transaction | 1,680.4 ms | **1,531.0 ms** | 1,578.4 ms |
| Redb commit | 657.4 ms | 492.4 ms | **458.9 ms** |
| packed publication/compaction | **262.7 ms** | 267.4 ms | 317.2 ms |
| process writes | 234.8 MiB | **196.8 MiB** | 197.5 MiB |
| peak ingest RSS growth | **142.1 MiB** | 172.7 MiB | 178.3 MiB |
| first row | 13.0 ms | **12.8 ms** | 13.1 ms |

Paired changes against the control:

| metric | 8,192 | 16,384 configured |
|---|---:|---:|
| completion throughput | **+4.8%** | +19.1% |
| store transaction | -13.7% | -23.1% |
| Redb commit | -19.9% | -39.5% |
| packed publication/compaction | +1.9% | +7.7% |
| process writes | -16.7% | -16.4% |
| peak ingest RSS | **+17.9%** | **+24.9%** |
| first row | -1.0% | -1.4% |

Redb commit variance was large enough that the `16,384` paired throughput
median and independent median point in opposite directions. That uncertainty
does not affect the decision: every setting must pass RSS as well as speed,
and the larger setting also fails to form the configured cohort.

## Mechanism conclusion

The current count ceiling is not arbitrary overhead. Larger atomic batches do
amortize durable Redb work, but they retain more parsed and resolver-owned event
state at once. At `8,192`, the saved commit time is partly consumed by packed
publication and the rest of the serial pipeline; the result is a small complete
gain bought with a material memory increase.

Do not retune the wait, queues, or byte ceiling inside this issue to rescue the
candidate. That would change several independent bounds and no longer answer
whether the count ceiling alone is a safe large lever.

## Consequence for epic #612

Redb commit remains material, but increasing the existing crash-atomic cohort
is not the missing multiplier. The durable storage path needs a representation
or commit strategy that reduces copy-on-write work without retaining a larger
owned event cohort. The non-storage MemoryStore ceiling also remains below the
epic gate, so a storage-only follow-up still cannot complete #612 by itself.
