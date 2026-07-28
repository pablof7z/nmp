---
title: Route preview and stalled-write observability
category: routing
slug: preview-and-observability
status: designed
date: 2026-07-29
owns:
  - Engine::preview_route and why it structurally cannot drift from real routing
  - preview's deliberate side effect on the discovery set
  - AwaitingRoute and the stalled_writes diagnostics section
  - why NOTHING auto-abandons — visibility replaces a give-up policy
related:
  - docs/internals/routing/auto-and-explicit.md
  - docs/internals/routing/resolution-lifecycle.md
  - docs/internals/routing/knowledge-and-settlement.md
  - docs/internals/routing/outbox.md
  - docs/internals/routing/resolvers.md
  - docs/internals/routing/removed-routes.md
  - docs/internals/writes/event-builder.md
  - docs/internals/writes/identity.md
issues:
  - "OPEN: an observable preview that re-emits as knowledge changes was NOT designed (§5)"
---

# Route preview and stalled-write observability

Both requirements in this document were derived from one case: a DM published
while the app was offline, for a recipient whose relay list turns out not to
exist. Pablo's ruling on it, in full — it contains the acceptance-time
impossibility argument, the preview requirement, and the observability
requirement in one paragraph:

> 4. Yeah, a DM that's been published while we were offline and we didn't know a dm relay list for that user doesn't exist it's a failure; there's no way to know that at acceptance time in the same way that we can't know if the user says "when you go online publish this to wss://non-existent.com" -- all we  can do is provide via the nip17 crate a "what will this compute to" so that apps can easily show which relays would be used for a certain communication, for example, such that they can disable the send button when a relay cannot be determined for one of the parties. This way of popping up a "we were trying to publish to relay X and it didn't work" or "we were trying to route this event but it didn't work" needs ato exist because there are many ways we'll find ourselves there

Two obligations fall out: a **preview** ("what will this compute to") so apps
can act before accepting, and **observability** ("we were trying to … and it
didn't work") because acceptance-time knowledge is impossible and failure
after acceptance is therefore normal, not exceptional.

**Status is marked per section.** BUILT sections describe current master
(`b99f9d41`); DESIGNED sections are settled design from the 2026-07-28/29
session, not yet implemented.

---

## 1. Why acceptance cannot know — DESIGNED (the position)

Neither an unreachable relay (`wss://non-existent.com`, explicitly named by
the user) nor an undiscoverable relay list (a recipient who never published a
kind:10050) is knowable at acceptance. The two failures are the same shape:
the obligation is well-formed, durably accepted, and only the world can
refuse it — later, asynchronously, possibly never with a definitive answer.

The design's response is deliberate: **do not pretend to know at acceptance,
and do not silently give up later.** Preview narrows the first gap where it
can (§2); observability makes the residue permanently visible (§4). What was
explicitly rejected is a give-up policy in between — see §4.4.

---

## 2. Preview cannot drift from real routing — DESIGNED

### 2.1 One derivation, one resolver

The central safety property here is structural, not procedural. Resolvers do
not take `&Event` — which would force a signed event into existence before
routing could be asked anything — they take

```rust
RouteSubject { kind, tags, author: Option<PublicKey> }
```

and BOTH callers build it: the send path from the accepted intent, the
preview from the unsigned builder. One derivation, one resolver, one
registry. There is no second "estimate" code path whose logic could rot
apart from the real one, so preview-says-X-send-does-Y is unrepresentable —
not tested away, but structurally absent. This is the same move the write
plane made for identity (a builder that cannot carry an author cannot
contradict one — `writes/event-builder.md`), applied to routing.

### 2.2 The surface

```rust
Engine::preview_route(&EventBuilder, Identity) -> RoutePreview {
    relays:   BTreeSet<RelayUrl>,  // what resolution yields right now
    complete: bool,                // false while unknowns remain
    blocked:  Vec<RouteBlock>,     // what is missing, per unknown, with why
}
```

A **pure read**: no acceptance, no journal row, no receipt, no lanes, nothing
durable. Calling it a thousand times changes no write-plane state.

### 2.3 The deliberate side effect: preview ARMS discovery

One exception to purity, chosen on purpose: preview contributes its
resolver-declared needs to the engine's discovery set (`resolvers.md` §4).
The contribution is widen-only — it can only add authors to the existing
discovery subscription's needed set, never narrow or tear down.

Consequence: opening a DM compose screen — which naturally previews to render
the send button — is what triggers the recipient's kind:10050 fetch. By the
time the user finishes typing, re-previewing has converged to
`complete: true`. Preview is not just a question; asking it starts producing
the answer. Re-preview on knowledge change and the loop closes (but see §5 —
push-based re-emission was not designed).

### 2.4 The call site this exists for

Pablo's own example, as a Swift compose screen:

```swift
let preview = try engine.previewRoute(builder: draft, identity: .active)
sendButton.isEnabled = preview.complete
if !preview.complete {
    footer.text = "Can't determine a relay for \(preview.blocked.first!.who)"
}
```

"Disable the send button when a relay cannot be determined for one of the
parties" — verbatim from the ruling. The nip17 crate's "what will this
compute to" is this call with a DM builder.

---

## 3. What already exists: granular delivery failure — BUILT

Post-routing failure is already observable per lane on master.
`WriteStatus` (`crates/nmp/src/outbox/mod.rs:32`) carries, among its
delivery states:

- `Rejected(RelayUrl, String)` — the relay's own refusal, with its message
  (`outbox/mod.rs:87`)
- `RetryEligible { relay, attempt, eligible_at }` — a failed attempt with
  the persisted ordinal and when the lane may retry (`outbox/mod.rs:64`)
- `GaveUp(RelayUrl)` — this lane exhausted its policy (`outbox/mod.rs:88`)
- `OutcomeUnknown(RelayUrl)` — an at-most-once attempt crossed a
  process-loss boundary after its Started fact committed; terminal
  ambiguity, never retry permission (`outbox/mod.rs:102`)

plus `AwaitingRelay`/`AwaitingAuth` for lanes parked on connectivity and
AUTH. "We were trying to publish to relay X and it didn't work" is served by
this machinery today. What is missing is everything BEFORE a relay exists —
the routing stage — which is §4.

---

## 4. Observability for what never got that far — DESIGNED

### 4.1 `AwaitingRoute { detail }`

A new retained, replayed `WriteStatus`: the intent is accepted (and possibly
signed) but resolution has not produced a single relay. `detail` carries the
resolver's stated reason ("no DM relay list known for npub…", "author relay
list never fetched"). Like `AwaitingCapability { pubkey }` — the shipped
precedent for a durable park that names what it waits for
(`crates/nmp/src/outbox/mod.rs:47`) — it is retained, not terminal, and
**re-emitted verbatim on receipt reattachment**, so a route parked for a
month is still visible with its reason a month later, across restarts. A park
nobody can see is indistinguishable from data loss; the detail string is the
difference between "stuck" and "stuck because X", and X is what the app can
act on.

This also fixes a verified defect: today a routing error at `on_signed`
terminally `Failed`s and drops the intent
(`crates/nmp/src/core/write.rs:2229-2243`), so publishing before the first
relay-list fetch dies permanently. The designed lifecycle parks instead
(`resolution-lifecycle.md`).

### 4.2 `stalled_writes` on `DiagnosticsSnapshot`

Per-write status answers "what happened to THIS write" — someone must be
holding the receipt to hear it. The global question, "is anything quietly
stuck", gets a new section on the EXISTING diagnostics snapshot
(`crates/nmp/src/core/diagnostics.rs:142`, facade mirror
`crates/nmp/src/diagnostics.rs:249`):

```rust
pub stalled_writes: Vec<StalledWrite>
// { receipt/intent identity, stage, detail, age }
```

covering all three stall classes: **unroutable** (`AwaitingRoute`),
**unsignable** (`AwaitingCapability`), and **undeliverable** (lanes parked or
exhausted with nothing progressing). `age` is what makes it a diagnostic: a
DM parked for 40 seconds is discovery in flight; parked for 40 days it is the
recipient-never-published-a-relay-list case, and only the app or user can
decide what that means. The `wss://non-existent.com` write lands here too —
routed instantly, undeliverable forever.

### 4.3 Nothing auto-abandons

Stated as sharply as the design meant it: **there is no give-up policy.
Visibility replaces it.** No TTL expires a parked route, no retry cap
terminally fails an unreachable relay's lane into oblivion, no heuristic
decides a recipient will "never" publish a relay list. The ruling's own
comparison is the argument: NMP can no more prove `wss://non-existent.com`
will never resolve than it can prove a 10050 will never appear — both are
open-ended facts about the world, and a durable queue that quietly drops
obligations on a guess is worse than one that holds them visibly. The app or
the user decides, with `stalled_writes` and `detail` as the evidence;
explicit cancellation remains the one abandonment door.

---

## 5. OPEN — a preview that re-emits

`preview_route` is poll-based: the compose screen re-calls it (on p-tag
change, on its own timer, on whatever the app chooses) and convergence relies
on §2.3 having armed discovery. An **observable preview** — a subscription
that re-emits `RoutePreview` as directory knowledge changes, so the send
button flips to enabled the moment the 10050 arrives without the app
polling — was raised and NOT designed. No contract exists for its lifecycle,
its interaction with the one-door `observe` rule, or its teardown. Design it
before building it; do not grow it ad hoc out of §2.3.
