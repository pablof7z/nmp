# Single-owner durable retry policy and deadline consumption

## Summary

Make the outbox the sole owner of durable EVENT retry, persist a bounded current-lane cursor, and expose retry deadlines only when one engine scheduler can consume and advance them.

## Boundaries

```mermaid
flowchart LR
  App[Write intent] --> Outbox[Outbox lane FSM]
  Outbox --> Store[(Lane cursor + eligibility + attempts)]
  Store --> Scheduler[Single engine scheduler]
  Scheduler -->|generation-scoped PublishAttempt| Transport[Transport socket worker]
  Transport -->|typed handoff result| Scheduler
  Scheduler --> Receipts[Receipt truth]
  Receipts --> SDKs[Rust / Swift / Kotlin]
```

## Detailed Plan

## Authority and current failure

Parent #79 and epic #23 define one owner for durable retry. Today the attempt table is history-only, the engine has volatile relay sets, and transport can retain queued EVENT commands across reconnect. That combination creates a second hidden publication queue. `next_deadline()` currently consumes only expiration and negentropy; adding retry timestamps alone would create a past-due busy loop.

## Unit #93 — transport handoff ownership

Add a correlated `PublishAttempt` command keyed by persisted attempt identity and connection generation. The typed result is exactly `NotHandedOff`, `Written`, or `Ambiguous`. Durable EVENT commands must be dropped and reported at generation end; they never enter the reconnect carry-over deque. `Written` means the socket write completed and is the only result that may later persist/emit `Sent`. `Ambiguous` never emits `Sent`; Durable waits for ACK then applies timeout policy, while AtMostOnce becomes `OutcomeUnknown` immediately. Keep REQ/subscription preamble replay unchanged. Prove rollover, disconnect-before-write, each handoff class, duplicate result, and unrelated read traffic.

Rollback: the PR is an internal seam and can be reverted before #95 consumes it. No persistence migration.

## Unit #94 — durable lane cursor and eligibility index

Add versioned `OUTBOX_LANES` keyed by length-prefixed `(intent, relay)`, ordered `OUTBOX_ELIGIBILITY` keyed by `(eligible_at, intent, relay)`, and additive `OUTBOX_ATTEMPT_DETAILS` keyed by the existing attempt key. Keep existing `OUTBOX_ATTEMPTS` rows at version 1 and immutable; timing, handoff, and transient classification live in the additive detail table so older binaries can still decode attempt rows. Add policy-free atomic doors for waiting, eligible, in-flight, transient, terminal, and terminal-intent-close transitions. Offline and AUTH waits have no eligibility row. All reads are bounded/indexed, using #87's discipline.

On first new-engine boot, deterministically insert missing lane cursors from open intents, route revisions, and the highest v1 attempt per lane: no attempt becomes waiting-for-connection; v1 terminal maps terminal; v1 Started maps a legacy in-flight state. The engine then atomically converts legacy Durable Started to interrupted/eligible and legacy AtMostOnce Started to `OutcomeUnknown`. The bootstrap is insert-if-absent and idempotent; it never rewrites v1 history.

Crash matrix: before/after lane creation, attempt start, handoff-detail write, terminal/transient finish, eligibility update, and open-intent close. Memory and Redb must be identical. Corruption fails closed and never deletes obligations. Retain receipts, routes, lanes, attempts, and details as evidence after open-work closure.

Rollback truth: the schema is additive and old binaries can decode unchanged v1 attempts, but behavioral rollback is unsafe while open intents have new lane state because the old engine cannot honor that cursor. Operational rollback therefore requires quiescing/closing open work or rolling forward; no destructive downgrade is claimed.

## Unit #95 — engine reducer and scheduler

Use typed states `WaitingConnection`, `WaitingAuth`, `Eligible`, `InFlight`, and terminal outcomes. One `schedule_ready(now)` path runs after boot, tick, connection/AUTH change, handoff result, OK, disconnect, cancellation, and persistence recovery. Stable order is `(eligible_at, intent, relay)`. Enforce 32 global and 1 per relay. Backoff is 3, 6, 12 seconds up to 300 seconds plus deterministic 0..<5-second jitter derived from persisted attempt identity. ACK timeout is 30 seconds.

Only actual committed starts consume an ordinal. Offline/AUTH waits consume none. `NotHandedOff` records that outcome without `Sent` and may safely re-arm either durability mode. `Written` is persisted before `Sent` and enters ACK wait. `Ambiguous` emits no `Sent`; Durable remains ACK-waiting and becomes transient at timeout, while AtMostOnce immediately becomes `OutcomeUnknown` and never re-enters eligibility. Durable interrupted attempts later dispatch under a new ordinal. Tick must consume due eligibility and ACK deadlines before computing the next deadline; when capacity is full, completion messages—not a zero deadline—wake scheduling.

NIP-20 classification uses only standardized machine prefixes, never free-form text: `duplicate` satisfies delivery and maps to Acked; `rate-limited` and `error` are transient; `auth-required` enters WaitingAuth; `invalid`, `pow`, `blocked`, and `restricted` are terminal Rejected. Unknown or malformed prefixes default to terminal Rejected and retain the raw reason, preventing infinite retries of possibly permanent invalidity.

Falsifiers cover no-deadline blocking, exact equality, cap-full past-due work, stable fairness, deterministic reopen, no polling, all handoff classes, every NIP-20 class/default, no hidden transport queue, persistence failure at every transition, exact bytes/ordinals, and bounded tick/effect counts.

## Unit #96 — governed receipt projection

Add `AwaitingRelay`, `AwaitingAuth`, `RetryEligible`, and `HandoffAmbiguous` plus ordinal/timing where required to canonical receipt facts. Emit `Sent` only after persisted `Written`, never for queue acceptance or ambiguity. Keep write retry truth distinct from query acquisition evidence. Update facade, UniFFI, Swift, Kotlin, direct-vs-FFI parity, exhaustive native mappings, both snapshots, and the exact append-only surface entry.

## Dependencies and coordination

#87 is merged. Prefer #88 before #95/#96 so corrupt retained evidence is typed. #86 may land independently but must rebase around core changes. #8 owns AUTH negotiation; #95 adds only the waiting/wake seam. #49 query evidence and #51 diagnostics remain separate. #81 requires repository-admin configuration and is not an implementation gate.

## Observability and acceptance

Receipts expose every wait/retry/handoff transition. Unchanged v1 attempt history plus additive detail rows preserve exact ordinals, timestamps, handoff classification, and outcomes. Tests assert the scheduler's visited/due work is bounded and the runtime blocks rather than polls. Completion requires all child PR tests plus workspace, Swift, Kotlin, surface regeneration, and trusted governance gates.

## Rule And ADR Check

- Complies with AGENTS.md issue-first discipline through parent #79 and children #93–#96; each implementation unit maps to one cohesive PR.
- Complies with VISION section 3.3 and bug-class ledger #16: transport owns sockets, outbox owns durable attempts, and one engine scheduler owns retry time and caps.
- Complies with the crash-safe Accepted plan correction: retry deadlines enter next_deadline only in the same unit as the transition that consumes and advances them.
- Complies with current store boundaries: persistence enforces atomic facts while the engine owns classification and policy.
- Complies with surface governance: #96 updates every platform, snapshots, parity tests, and the append-only change log together.

## Possible Rule Or ADR Loosening

- No existing rule should be loosened. In particular, transport must not regain an implicit durable EVENT queue, and failed persistence must not produce wire or terminal facts.

## Possible Rule Tightening

- Consider adding a durable rule that every deadline source must name the state transition that consumes and advances it.
- Consider requiring every durable transport handoff to be correlated, generation-scoped, and classified as proven-not-handed-off or ambiguous.

## Alternatives Considered

- Reuse transport reconnect backoff as publication retry policy: rejected because it creates duplicate ownership and cannot preserve attempt ordinals or AtMostOnce ambiguity.
- Reconstruct current retry state by scanning attempt history: rejected because it is unbounded, complicates crash truth, and conflicts with #87's indexed-range discipline.
- Add retry timestamps to next_deadline before the reducer exists: rejected because a past-due value would wake repeatedly without advancing state.
- Expose waits only in diagnostics: rejected by owner decision; receipts now carry AwaitingRelay, AwaitingAuth, and RetryEligible truth across all SDKs.
- Prune terminal history in the retry implementation: rejected; open working rows close, but retained evidence waits for an explicit GC policy.

## Certainty

94 percent.

## Decision

ready

## Hosted Artifacts

- Plan page: Generated after publishing.

- TTS audio: https://blossom.primal.net/91606d34f8898ecf574b2006a95c6bb05a1a9e2897f0d81ae4cf6eec455dce09.mp3
