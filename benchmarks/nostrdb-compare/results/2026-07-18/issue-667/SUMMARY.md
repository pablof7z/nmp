# Issue #667 — committed history delivery attribution

## Decision

Close the history/live-query delivery hypothesis negative and do not ship the
candidate.

The dominant work inside committed history projection was transiently cloning
every inserted event before truncating to the bounded 200-row window. Ranking
borrowed candidates before materialization reduced Redb's projection median
from 196.0 ms to 29.2 ms and reduced explicit event clones from 100,000 to
5,336. Despite removing 85.1% of that phase, complete Redb throughput improved
only 6.0%, below #667's 10% production gate. MemoryStore improved 1.9%.

The candidate was therefore reverted. The benchmark-only attribution remains
so future production probes can distinguish projection, sink, channel, and
receiver work.

## Representative production result

Every fresh process crossed websocket delivery, JSON parsing, signature
verification, resolver mutation, the selected store, bounded history
projection, observer delivery, and exact completion. All reports observed
100,000 relay events and ended with exactly 200 visible rows.

| Metric | Redb baseline | Redb candidate | Change | Memory baseline | Memory candidate | Change |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| throughput | 40,125 events/s | 42,518 events/s | **6.0% higher** | 84,651 events/s | 86,238 events/s | **1.9% higher** |
| complete ingest wall | 2,492.2 ms | 2,351.9 ms | 5.6% lower | 1,181.3 ms | 1,159.6 ms | 1.8% lower |
| committed history projection | 196.0 ms | 29.2 ms | **85.1% lower** | 240.7 ms | 175.3 ms | 27.2% lower |
| explicit projection clones | 100,000 | 5,336 | **94.7% lower** | 100,000 | 35,600 | 64.4% lower |
| peak RSS growth | 148.6 MB | 149.3 MB | 0.5% higher | 681.9 MB | 681.7 MB | unchanged |
| first row | 13.9 ms | 13.3 ms | 4.3% lower | 2.9 ms | 2.7 ms | 7.4% lower |

Values are independent medians of 3 fresh processes. Redb commit variance was
visible in the baseline runs, but the favorable candidate still cannot meet
the gate: its measured median gain is 6.0%, and deleting the baseline's entire
196 ms reducer phase would save only 7.9% of median complete wall.

## Attribution

On the Redb baseline, committed history projection consumed 196.0 ms. Its
median internal owners were:

- setup and affected selection: 0.1 ms;
- state mutation and transient row materialization: 170.0 ms;
- delta construction: 14.9 ms;
- complete bounded-frame construction: 2.5 ms;
- the core sink clone/delivery: 6.1 ms;
- runtime mailbox publication: effectively zero;
- receiver-side reconciliation, outside the engine acknowledgement: 17.7 ms.

The result rules out history delivery as the next 10% Redb lever. The larger
remaining owner is still the governed store path. Its current timing must be
split between packed-postings publication/compaction and Redb's physical
commit before an engine migration is justified.
