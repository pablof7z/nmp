# Issue #663 committed-duplicate ceiling

## Decision

Proceed to a production-safe prototype, with a hard negative close if its
three-run replay median falls below 500,000 frames/s.

The production-shaped favorable ceiling reached a median 639,563 replay
frames/s across three fresh processes, 27.9% above the epic gate. Every run
fingerprinted the exact raw EVENT object independently of subscription id,
hit all 100,000 second-pass observations, and performed no second-pass Event
parse, resolver materialization, or store transaction.

This is deliberately not production behavior. The diagnostic cache publishes
on the first successful parse, before durable commit, and the lightweight hit
token does not retain raw bytes for a race fallback. Its purpose is to settle
whether bypassing the full ingest path has enough ceiling to justify the exact
publication/invalidation protocol.

## Measurement correction

Schema v11 gates pass 2 until diagnostics and the visible projection prove
that pass 1 has been applied. The previous back-to-back server measured the
tail of initial ingest as replay time, which cannot evaluate a cache whose
eligibility is published only after durable application.

The completion observer now polls at 1 ms rather than 100 ms. Duplicate hits
advance diagnostics without emitting row deltas, so the old interval could
quantize a sub-200-ms replay into one or two whole observer sleeps.

## Results

| Run | Replay ms | Replay frames/s | Exact hits | Parsed frames | Resolver events | Store events |
|---|---:|---:|---:|---:|---:|---:|
| 1 | 156.357 | 639,563 | 100,000 | 100,001 | 100,000 | 100,000 |
| 2 | 155.194 | 644,353 | 100,000 | 100,001 | 100,000 | 100,000 |
| 3 | 177.519 | 563,319 | 100,000 | 100,001 | 100,000 | 100,000 |
| **Median** | **156.357** | **639,563** | **100,000** | **100,001** | **100,000** | **100,000** |

`parsed_frames` includes the 100,000 first-pass EVENTs plus EOSE. Resolver and
store event counts cover only pass 1. Diagnostics counted all 200,000 EVENT
frames in every run and exact reopen retained 100,000 canonical events.

The run used a 50 us engine-batch coalescing wait. A single no-wait ceiling run
reached 621,908 frames/s; 50 us reduced bridge fragmentation and produced the
stronger matrix without changing the bounded batch or byte ceilings. This
setting remains a candidate until ordinary-ingest controls prove it does not
regress latency or throughput.

## Remaining production cost

The median has useful but narrow headroom. Production still needs:

- post-commit publication keyed by `(RelayUrl, BLAKE3(raw EVENT bytes))`;
- bounded slot epochs and EventId reverse invalidation;
- an owned raw-frame fallback after lookup/invalidation or pending-intent races;
- ordered mixed full-frame/hit runs; and
- current session/generation, diagnostics, and applied-ack preservation.

The diagnostic token itself allocates one boxed relay-message wrapper per hit;
a native production token can move tungstenite's owned text and metadata
directly. That avoids one artificial ceiling cost, but the production cache
adds synchronization and revalidation. The measured production prototype,
not this ceiling, decides the issue.
