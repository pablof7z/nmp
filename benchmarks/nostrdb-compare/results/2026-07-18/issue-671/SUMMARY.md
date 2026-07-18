# Issue #671 — verifier lane batching result

## Decision

Close verifier lane batching negative and do not ship the candidate.

The current verifier batch API sends one task and receives one result message
per event. The candidate partitioned each batch into at most one task per
worker. It reduced task and result messages from 100,000 to a median 1,585 on
MemoryStore, but complete MemoryStore throughput improved only 0.2%. Redb
throughput was 1.7% lower. The 10% production gate was not approached, so the
candidate was reverted.

The benchmark-only attribution remains. It distinguishes caller dispatch,
caller collection/wait, and summed worker Schnorr execution.

## Complete production pipeline

Every fresh process crossed websocket delivery, JSON parsing, ID validation,
required Schnorr verification, resolver mutation, the selected store, bounded
history projection, observer delivery, and exact completion. All reports
observed 100,000 events and ended with exactly 200 visible rows.

| Metric | Redb baseline | Redb candidate | Change | Memory baseline | Memory candidate | Change |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| throughput | 39,497 events/s | 38,842 events/s | 1.7% lower | 84,864 events/s | 85,062 events/s | 0.2% higher |
| complete ingest wall | 2,531.9 ms | 2,574.5 ms | 1.7% higher | 1,178.4 ms | 1,175.6 ms | 0.2% lower |
| verifier batch wall | 984.9 ms | 1,029.4 ms | 4.5% higher | 894.1 ms | 866.1 ms | 3.1% lower |
| task submissions | 100,000 | 1,640 | 98.4% lower | 100,000 | 1,585 | 98.4% lower |
| result messages | 100,000 | 1,640 | 98.4% lower | 100,000 | 1,585 | 98.4% lower |
| peak RSS growth | 147.8 MB | 164.7 MB | 11.5% higher | 682.7 MB | 665.7 MB | 2.5% lower |
| first row | 13.6 ms | 13.7 ms | unchanged | 3.4 ms | 3.1 ms | 8.1% lower |

Values are independent medians of 3 fresh processes. Redb commit variance is
visible in both matrices, but the stable MemoryStore control is decisive: the
candidate changed complete throughput by only 0.2%.

## Attribution

The MemoryStore baseline spent a median 894.1 ms inside verifier batch calls.
Only 4.5 ms was caller-side task dispatch. The 889.5 ms collection bucket is
mostly the caller waiting for workers, not result-message CPU. Summed worker
Schnorr execution was 5,574.0 ms across 8 lanes, or an ideal perfectly balanced
floor near 697 ms before caller coordination.

Lane batching reduced summed worker time to 4,792.3 ms and verifier wall to
866.1 ms, but that small wall reduction did not move complete throughput. The
per-event channel shape was therefore conspicuous but not load-bearing.

## Consequence for #612

Do not add verifier batching machinery. The remaining storage-free ceiling is
owned by required Schnorr computation plus JSON parse/ID construction, not
per-event channel traffic. A next experiment must attack those costs directly
and must first prove a favorable complete-pipeline ceiling; clone or scheduler
cleanup cannot plausibly reach the epic target.
