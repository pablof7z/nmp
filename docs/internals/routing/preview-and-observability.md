---
title: Route preview and stalled-write observability
category: routing
slug: preview-and-observability
status: partial
date: 2026-07-29
owns:
  - the routing park, its reason, and the stalled_writes diagnostics section
  - why nothing auto-abandons a stalled write
  - "`GaveUp` is terminal, not resumable (owner ruling, 2026-08-17)"
related:
  - docs/internals/routing/auto-and-explicit.md
  - docs/internals/routing/resolution-lifecycle.md
  - docs/internals/routing/knowledge-and-settlement.md
  - docs/internals/routing/outbox.md
  - docs/internals/routing/resolvers.md
  - docs/internals/writes/event-builder.md
  - docs/internals/writes/identity.md
issues:
  - "OPEN: #1031 proposes a delivery-attempt ceiling for a lane; unreconciled with this doc's no-auto-abandon position (§4.3)"
---

# Route preview and stalled-write observability

Both requirements in this document were derived from one case: a DM published
while the app was offline, for a recipient whose relay list turns out not to
exist. Pablo's ruling on it, in full — it contains the acceptance-time
impossibility argument, the preview idea, and the observability requirement
in one paragraph:

> 4. Yeah, a DM that's been published while we were offline and we didn't know a dm relay list for that user doesn't exist it's a failure; there's no way to know that at acceptance time in the same way that we can't know if the user says "when you go online publish this to wss://non-existent.com" -- all we  can do is provide via the nip17 crate a "what will this compute to" so that apps can easily show which relays would be used for a certain communication, for example, such that they can disable the send button when a relay cannot be determined for one of the parties. This way of popping up a "we were trying to publish to relay X and it didn't work" or "we were trying to route this event but it didn't work" needs ato exist because there are many ways we'll find ourselves there

Two obligations fall out of it: a **preview** ("what will this compute to")
so apps can act before accepting, and **observability** ("we were trying to …
and it didn't work") because acceptance-time knowledge is impossible and
failure after acceptance is therefore normal, not exceptional. The preview
half of this record was never built — see §2. The observability half is
built, under different names than this document originally used — see §3
and §4.

---

## 1. Why acceptance cannot know (the position)

Neither an unreachable relay (`wss://non-existent.com`, explicitly named by
the user) nor an undiscoverable relay list (a recipient who never published a
kind:10050) is knowable at acceptance. The two failures are the same shape:
the obligation is well-formed, durably accepted, and only the world can
refuse it — later, asynchronously, possibly never with a definitive answer.

The design's response is deliberate: **do not pretend to know at acceptance,
and do not silently give up later.** What was explicitly rejected is a
give-up policy in between — see §4.3.

---

## 2. No write-side route-preview API exists

`preview_route`, `RoutePreview`, `RouteBlock`, `RouteSubject`, and
`observeRoutePreview` are not built. There are zero hits for any of them
across `crates/` and `Packages/`. (`nmp-router`'s `preview_admission` /
`AdmissionPreview` is a different, read-side subsystem — it answers a
different question and is not this.)

The use case the ruling above asked for is unserved: an app has no way to
ask "where would this event route to" before accepting a write, and so
cannot do what Pablo's example described —

```swift
let preview = try engine.previewRoute(builder: draft, identity: .active)
sendButton.isEnabled = preview.complete
if !preview.complete {
    footer.text = "Can't determine a relay for \(preview.blocked.first!.who)"
}
```

— disabling a compose screen's send button when a relay cannot be determined
for one of the parties. This is illustrative of the gap, not a specification
of an API to build: no `previewRoute`, `preview.complete`, or
`preview.blocked` exists anywhere in the codebase today.

---

## 3. Post-routing delivery failure: built, under different names

Once a write has a relay, failure at that relay is observable on master
today. The type is `RelayState` (`crates/nmp-engine/src/publish_queue/mod.rs:181-221`):

- `Rejected { reason: String }` — the relay authenticated the identity and
  refused this event (mod.rs:206-208).
- `AuthFailed { pubkey, source, reason }` — the write could not be
  authenticated at this relay.
- `GaveUp` — a bare unit variant: the attempt ceiling was reached at this
  relay. **Terminal, not resumable — owner ruling, 2026-08-17: "give up is
  obviously final."** The lane is done; nothing later reopens it, and the
  promise holds across a restart because the state is durable. The
  recommendation put to the owner was the opposite — resumable, on the
  grounds that NMP could not keep a terminal promise across a restart — and
  it was wrong on its own facts as well as overruled. What remains open
  under #1031 is how a lane REACHES this state (attempt count versus
  wall-clock deadline), which is a separate question and does not soften
  this one (see §4.3).
- `Waiting(RelayWaiting)` — not connected, needs AUTH, eligible-and-queued,
  or backing off after a retryable failure. What this document's earlier
  draft called `AwaitingRelay`/`AwaitingAuth` are really
  `RelayWaiting::NotConnected` / `RelayWaiting::NeedsAuth`
  (mod.rs:143, 146). What it called `RetryEligible { relay, attempt,
  eligible_at }` is really `RelayWaiting::BackingOff { attempt, eligible_at,
  cause, detail }` (mod.rs:165-171) — `cause`/`detail` say WHY the relay is
  being retried, not just that it is.

`OutcomeUnknown` is **not built**. It does not exist as a variant anywhere
in this enum or a sibling one; it survives only as a comment
(`crates/nmp-transport/src/pool.rs:193`) describing an at-most-once attempt
that crossed a process-loss boundary after its Started fact committed,
deferred to issue #95: "Durable durability waits for an ACK/timeout policy
(#95); `AtMostOnce` becomes `OutcomeUnknown`". Until #95, that ambiguity is
not surfaced as a typed delivery state.

"We were trying to publish to relay X and it didn't work" is served by this
machinery today, once a relay is known. What is missing is everything
BEFORE a relay exists — the routing stage — which is §4.

---

## 4. Observability for what never got that far

### 4.1 The routing park and its reason — built

`WriteFact::Destinations { relays, complete, awaiting_author_routes }`
(`crates/nmp-engine/src/publish_queue/mod.rs:337-341`): the intent is
accepted (and possibly signed) and resolution has not closed. It is
retained, not terminal, and **replayed on receipt reattachment**, so a route
parked for a month is still visible a month later, across restarts. A park
nobody can see is indistinguishable from data loss.

The reason is a SET OF KEYS, not a sentence, and that is the load-bearing
part. `awaiting_author_routes` names every author whose routes the write is
still waiting on; a later positive route fact for any one of them is the only
thing that can move the picture, so the same value is both the reason and the
list of repairs.

This section previously specified `AwaitingRoute { detail: String }`, which
shipped, and the string was the defect. "Still determining destinations" and
"nowhere to send this" arrived as one rendered English sentence that no
program could branch on — an app that wanted to tell them apart had to
prefix-match prose (#1236). The two halves are now separate typed facts:
`complete` is the branch (knowledge exhausted or not, never delivery), and
`awaiting_author_routes` is the detail behind the open side of it. Anything
rendered belongs above this layer, in the app, which is where the language
and the audience are known.

The precedent still holds and is now literal rather than analogical:
`SigningState::AwaitingSigner { pubkey }` is the same shape — a durable park
that names the key it waits for, in the decoded type
(`docs/internals/conventions/bech32-boundary.md`).

This also fixed a verified defect that predated this record: a routing error
at `on_signed` used to terminally `Failed` the intent and drop it, so
publishing before the first relay-list fetch died permanently. The lifecycle
now parks instead (`resolution-lifecycle.md`).

### 4.2 `stalled_writes` on `DiagnosticsSnapshot` — built

Per-write status answers "what happened to THIS write" — someone must be
holding the receipt to hear it. The global question, "is anything quietly
stuck", has a section on `DiagnosticsSnapshot`
(`crates/nmp-engine/src/core/diagnostics.rs:264`, field at
`diagnostics.rs:307`; facade mirror `crates/nmp/src/diagnostics.rs`):

```rust
pub stalled_writes: Vec<StalledWrite>
```

`StalledWrite` is defined at `crates/nmp-engine/src/core/diagnostics.rs:188`.
It covers all three stall classes: **unroutable** (an open, empty
destination picture), **unsignable** (`AwaitingCapability`), and
**undeliverable** (lanes parked or exhausted with nothing progressing).
`age` is what makes it a diagnostic: a DM parked for 40 seconds is discovery
in flight; parked for 40 days it is the recipient-never-published-a-relay-list
case, and only the app or user can decide what that means. The
`wss://non-existent.com` write lands here too — routed instantly,
undeliverable forever.

### 4.3 Nothing auto-abandons

**There is no give-up policy on the routing park. Visibility replaces it.**
No TTL expires a parked route, no retry cap terminally fails an unreachable
relay's lane into oblivion, no heuristic decides a recipient will "never"
publish a relay list. The ruling's own comparison is the argument: NMP can no
more prove `wss://non-existent.com` will never resolve than it can prove a
10050 will never appear — both are open-ended facts about the world, and a
durable queue that quietly drops obligations on a guess is worse than one
that holds them visibly. The app or the user decides, with `stalled_writes`
and `detail` as the evidence; explicit cancellation remains the one
abandonment door.

**Unresolved against this: #1031 proposes a delivery-attempt ceiling, which
is a give-up policy for the lane.** The owner ruled on 2026-08-17 that when
NMP does give up, that is final — "give up is obviously final" (§3) — which
settles what `GaveUp` MEANS but not whether this section's no-ceiling
position survives. The two are stated here side by side rather than
reconciled by guess: this section is about routing parks, #1031 is about a
lane that has a relay and cannot reach it, and nobody has ruled on whether
that distinction holds. Neither should be cited as having settled the other.
