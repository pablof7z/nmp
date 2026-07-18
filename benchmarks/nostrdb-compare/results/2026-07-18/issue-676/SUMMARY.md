# Issue #676 — relay-frame planning attribution

## Decision

Do not ship either relay-session candidate.

Reusing the already-reported session after exact validation reduced allocation
calls 6.7% and allocated bytes 6.5%, but complete MemoryStore throughput was
1.8% lower. Sharing one session allocation across pool/engine frames and
batching per-session/per-kind diagnostics made the measured session-validation
phase 69.0% cheaper, but complete throughput was 0.5% lower and allocations
grew. Both production candidates were reverted under the issue's 10% gate.

The result corrects #675's first attribution inference: the residual interval
is real, but the obvious session clone and diagnostics-map work are not its
material owner.

## Complete MemoryStore result

Values are independent medians of 3 fresh processes over the exact 100,000
frame pipeline.

| Mode | Median throughput | Change | Median allocation calls | Median allocated bytes |
| --- | ---: | ---: | ---: | ---: |
| Attributed baseline | 85,722 events/s | — | 12,184,845 | 3.47 GB |
| Reuse validated session | 84,173 events/s | 1.8% lower | 11,373,304 | 3.25 GB |
| Shared session + batched counts | 85,310 events/s | 0.5% lower | 12,422,494 | 3.53 GB |

Every run observed exactly 100,000 frames and ended with exactly 200 visible
rows. The Redb matrix was not run because neither candidate passed the
MemoryStore gate.

## What the CPU attribution proves

The final reverted baseline compares wall time with engine-thread CPU time,
so queue backpressure and scheduler preemption no longer masquerade as reducer
work.

At the medians, the engine reducer consumed 1,209.7 ms of thread CPU. The full
`ingest_relay_observations` call consumed 815.8 ms, leaving 393.9 ms in the
surrounding relay-frame reduction path — 32.6% of reducer CPU.

The named per-frame phases account for only about 71.8 ms:

- typed EVENT extraction: 40.5 ms;
- exact handle/session validation: 9.1 ms;
- diagnostics counting: 12.5 ms;
- provenance/candidate construction: 9.7 ms.

About 322 ms therefore remains outside those named operations. The two
production experiments show it is not recoverable by eliminating session
clones or merging diagnostics counters. It includes loop/ownership destruction
and benchmark-attribution overhead around the batches; treating it as a single
optimizable production function would be false precision.

## Consequence for #612

Stop pursuing relay-session ownership as the next multiplier. The largest
measured semantic owner remains the resolver/store/committed-projection path,
and #675 already shows that parser, crypto, history, and verifier ceilings only
become useful in combination. The next work should use complete thread-CPU
boundaries around resolver classification, store insertion, and committed
projection before selecting another production candidate.
