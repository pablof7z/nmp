# Issue #646 — redundant author-kind index result

NMP no longer persists a dedicated `BY_AUTHOR_KIND` row or sampled
author-kind cardinality row for every event. An author+kind filter chooses the
smaller sampled author or kind index and applies the other predicate to the
borrowed canonical event value. Query results and canonical ordering remain
exact.

Schema v7 migrates v6 atomically: it drops the retired table, rebuilds the
planner sidecar without the retired namespace, then publishes the new schema
marker in the same Redb transaction. Healthy v7 reopen remains read-only.

## Governed store import

Fifteen clean-process pairs alternated merged baseline `30ad22b` and the
candidate over the representative 100,000-event corpus. Every child reopened
exactly 100,000 canonical rows and returned identical query row counts.

| Metric | Baseline | Candidate | Paired result |
| --- | ---: | ---: | ---: |
| median import wall | 2,596 ms | 1,935 ms | 0.790x |
| derived throughput | — | — | **26.6% faster** |

## Production relay pipeline

Five clean-process pairs crossed the real websocket, JSON parse, signature
verification, resolver, governed store, and bounded live-query path. Stores
were fresh files on `/dev/md1`; run order reversed each repetition.

| Metric | Paired candidate / baseline | Result |
| --- | ---: | ---: |
| relay ingest throughput | 1.133x | **13.3% faster** |
| process writes | 0.747x | **25.3% lower** |
| store transaction time | 0.801x | 19.9% lower |
| ordered/tag index time | 0.631x | 36.9% lower |
| peak RSS | 0.949x | 5.1% lower |
| first-row latency | 1.042x | 4.2% higher |

All ten pipeline children observed exactly 100,000 relay frames. The candidate
median was 26,656 events/s versus 23,207 events/s for the immediately preceding
merged baseline.

## Query falsifier

Five clean-process pairs ran 50 iterations of complete kind, author,
author+kind, tag, two-tag, and 43-author queries plus bounded kind and tag
queries. Every result count was identical. The worst paired-median p95 ratio
was 1.066 for the complete one-row author query. The author+kind query itself
was 1.004x at p95. Both remain below the 10% regression gate.

## Decision

Merge the removal. It eliminates one durable mutation per event, reduces real
pipeline writes by a quarter, and improves production throughput without
weakening query semantics. This is another meaningful #627 slice, not closure:
the production result remains far below #612's 150,000 events/s gate and
durable store work remains the dominant constraint.

