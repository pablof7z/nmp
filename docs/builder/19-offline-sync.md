# Offline, reconnect, and acquisition evidence

**Status: CURRENT + TARGET.** Capability-probed negentropy, persisted
per-(filter, relay) watermarks, cache replay, and subscription replay are built.
The target contract replaces aggregate completeness language and adds a
crash-safe durable outbox with logical retry. Those additions are not shipped
yet.

After this chapter you will know what survives offline use, what a relay
watermark actually proves, and which layer owns each kind of retry.

## Negentropy is capability-gated

NIP-77 negentropy reconciles sets without replaying every stored event. NMP
uses it only for a relay that has produced a `ProbedRelay` capability token.
The token has no public constructor, and the negentropy effect requires one:

```rust
NegOpen(ProbedRelay, SubId, ConcreteFilter, String)
```

A bare `RelayUrl` cannot enter that path. Unprobed or unsupported relays use a
plain REQ. Limited filters also use REQ because relay-side limiting does not
compose safely with set reconciliation or acquisition attribution.

This is bug-class ledger #8: unsupported negentropy is excluded by the type
shape, not by an app convention.

## Watermarks are source evidence, not global truth

EOSE or a completed reconciliation proves that one relay finished one request
shape for one window. NMP persists that fact because it is useful for offline
cache delivery, restart, avoiding redundant work, and diagnostics.

It does not prove that every matching event on Nostr has been found. A private,
unknown, LAN, slow, or currently offline relay may also hold matching data.

The shipping API currently aggregates relay facts into:

```text
Coverage = Unknown | CompleteUpTo(watermark)
```

Read `CompleteUpTo` narrowly: the sources in the current plan met the current
aggregation rule up to that watermark. Do not render it as globally complete
or authoritative empty. The target query snapshot instead returns cached rows
with compact per-planned-source acquisition and shortfall evidence. Exact
relay, AUTH, EOSE, error, and watermark facts remain in diagnostics.

An evidence-only change may still emit a query snapshot with no row mutation.
For example, one planned relay may reach EOSE while another becomes
AUTH-blocked. The app receives those facts and decides its own UX; NMP does not
collapse them into `syncHealth` or `synced`.

## What survives a reconnect today

When a relay connection is replaced, the current runtime:

1. discards attribution tied to the stale connection generation;
2. replays still-live planned REQs on the new generation;
3. reuses a cached NIP-77 capability verdict where valid; and
4. reacquires per-source evidence as replies arrive.

The app does not reopen subscriptions or run a polling loop. Live demand is
derived from query handles; transport reconnection restores the wire work for
that demand.

An in-progress negentropy exchange is connection-local and is not resumed byte
for byte. The next eligible compile/reconnect establishes fresh work for the
still-live demand.

## Writes have a different durability contract

Subscription replay and write retry are not the same mechanism.

### Current implementation

- Ephemeral writes are fire-and-forget and are not replayed.
- Current durable/at-most-once receipts report what happened in this process,
  but the outbox does not yet persist the full accepted obligation, attempt
  journal, or retry eligibility required by the target contract.
- A disconnect may therefore end a current in-flight relay lane as `GaveUp`.

### Target contract

- Transport owns socket reconnection only. It never hides a second durable
  EVENT buffer.
- The durable outbox owns each persisted `(intent, relay)` lane, including the
  exact signed bytes, attempt ordinal, outcome, and `nextEligibleAt`.
- One deadline scheduler wakes eligible work with logical backoff. It sleeps to
  real deadlines; there is no fixed-rate polling.
- Offline or AUTH-blocked time does not consume an attempt.
- A transient delivery failure advances backoff; a relay ACK or permanent
  rejection closes that lane.
- At-most-once ambiguity becomes `OutcomeUnknown` and is never blindly retried.

Dropping a receipt observer does not cancel a target durable intent. Its facts
are persisted and reattachable by receipt id after restart.

## Cold-start offline behavior

On an offline launch, NMP returns matching cached rows immediately with the
persisted evidence available for them. The app may show stale data, an offline
indicator, or an empty local result according to product policy. NMP does not
call the cache "the truth."

After the app recreates its live queries, reconnect and sync continue from the
persisted source facts. Query object lifetimes are still app-owned; NMP does not
install scene-phase machinery or silently recreate an app's query set.

## Current gaps

- The negentropy liveness sweep exists against an injected clock but is not yet
  driven by the unified deadline scheduler.
- An already-open plain REQ is not upgraded the instant a NIP-77 probe succeeds;
  the next relevant recompile chooses the capability-gated path.
- Crash-safe `Accepted`, persisted signer waiting, durable receipt replay, and
  logical retry remain target work. See
  [Durable writes, signing, and retry](../design/durable-write-signing-and-retry.md).

---

<!-- nav-footer -->
<sub>← [Tracing demand](18-tracing-demand.md) · [Index](README.md) · [Capabilities](20-capabilities.md) →</sub>
