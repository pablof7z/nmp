# Issue #663 committed duplicate fast path

## Production decision

Select the crash-safe committed-observation fast path.

All three fresh production runs cleared the 500,000 replay frames/s gate. The
median was 534,503 frames/s, 6.9% above the gate. Every replay observation hit
the bounded cache, while parsing, signature verification, resolver
materialization, and store work remained confined to the first pass.

| Run | Replay ms | Replay frames/s | Safe hits | Parsed frames | Resolver events | Store events |
|---|---:|---:|---:|---:|---:|---:|
| 1 | 187.090 | 534,503 | 100,000 | 100,001 | 100,000 | 100,000 |
| 2 | 195.987 | 510,238 | 100,000 | 100,001 | 100,000 | 100,000 |
| 3 | 173.781 | 575,438 | 100,000 | 100,001 | 100,000 | 100,000 |
| **Median** | **187.090** | **534,503** | **100,000** | **100,001** | **100,000** | **100,000** |

This is the production protocol, not the favorable ceiling. Cache entries are
published only after the governed transaction commits. A token retains the
exact websocket text for fallback and is accepted only after current-session,
slot-epoch, and pending-write revalidation. Every canonical removal
invalidates all observations for that EventId before effects are published.
Mixed ordinary and cached frames retain wire order through commit barriers.

## Acceptance gates

- The production median is 16.4% below the unsafe 639,563 frames/s ceiling,
  but remains above the required gate in every run.
- A 200 us bounded engine coalescing wait produced 41,330 events/s median
  first-seen throughput, 11.8% above the same candidate with no wait. Against
  #661's fresh 42,234 events/s Redb baseline, the candidate is 2.1% lower and
  remains inside the epic's 10% regression allowance.
- The one-million scale run observed, committed, and reopened exactly
  1,000,000 events. It completed at 21,633 events/s with 184,774,656 bytes of
  peak RSS growth. That RSS result is 9.9% above #650's packed-Redb
  one-million qualifier and remains inside the 10% allowance.
- `nmp-store` has no source diff in this change. The exact selective-query
  implementation and packed representation qualified by #650 are unchanged;
  this transport/engine optimization cannot alter store query planning or
  result order.
- The cache is volatile, globally bounded to 131,072 observations by default,
  and has no persistent schema or recovery obligation. Eviction, restart,
  invalidation, poisoned synchronization, and malformed frames all take the
  ordinary exact path.

The 200 us wait is now the production default because it improved both replay
batching and ordinary first-seen throughput in the controlled matrix.

## Favorable ceiling history

## Decision

The favorable ceiling justified the production prototype. The production
result above now supersedes this intermediate decision.

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
