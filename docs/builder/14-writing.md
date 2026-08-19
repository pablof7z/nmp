# Writing: acceptance, pending rows, and receipts

A write is an intent with an observable receipt, not a call that returns one
success boolean.

## Publish an event

Publishing takes a `WriteIntent` naming the event payload (kind, tags,
content) and its routing — `.auto` says nothing and lets NMP route it.

The payload is unsigned until NMP signs it. The engine or an enabled protocol
module may validate closed typed context, but no stage mutates an already
signed event — `.signed(...)` publishes bytes that already carry a signature,
verbatim.

- `identity` defaults to the current account and applies to this intent only.
- typed protocol context may contribute route/access facts.
- the app does not expand ordinary routing into relay arrays; it either names
  relays or says nothing (`17-relays.md`).

There is no `NMPDraft`, no `durability` selector, no `signer` field and no
`context` field. Those spellings are deleted, not renamed.

## Where this surface is going — owner ruling, 2026-08-17

The shape above is what ships. The owner ruled on 2026-08-17 that it collapses
further, and the ruling is recorded here so the next person to touch this
surface does not re-derive it. He wrote the target twice, first as an example:

> yeah, WriteIntent should be able to take the relay list to publish to like
> `nmp.publish(eventBuilderOutput, ['relay1', 'relay2'])` -- right?

and then as the signature:

> `publish(event, relays?, signer?)` ?

So a publish takes **the event**, **optionally the relays**, and **optionally
who signs it**. Nothing else. Three consequences he stated directly:

- **Naming no relays is how an app asks NMP to route it.** *"do we really need
  to be explicit about this? can't it just be the lack of `relays: ["relay1",
  "relay2"]` be enough to determine this is to be automatically routed?"* An
  absent relay list is the whole of that signal; a separate routing word is
  not carrying information the relay list does not already carry.
- **The wrappers around those values go.** Told that if an absent relay list
  means "route it", then the read-routing and write-routing types and half of
  `Demand` become unnecessary, he answered *"yes, good! they should! they seem
  very stupid"*. On the surrounding ceremony — *"is all this boilerplate
  needed? can't it just literally be a simple array of relays and that's
  it?"*, and of `WritePayload`, *"what is this WritePayload thing? sounds
  pretty boilerplaty to me; does it have any purpose?"*
- **Who signs is an optional argument, not something a capability decides.**
  Same day, same subject: `nmp.follow(bob, as: alice)` or `nmp.follow(bob)`.
  See `docs/internals/writes/identity.md` §7.

This is a ruling on direction, not a specification: it does not say what the
collapsed types are called, or where the pieces that currently carry
authenticated identity and payload variants land afterwards.

## Durable acceptance is a transaction

For a durable write, `accepted(intentId)` is emitted only after one crash-atomic
commit owns:

- the frozen NIP-01 body, expected pubkey, and final event id;
- a stable intent/receipt id allocated so it cannot be reused after restart;
- the pinned signer identity reference and durability policy;
- the canonical local `pending(intentId)` row;
- any replaceable winner displaced by that row;
- the open delivery obligation and known route/retry state; and
- the receipt history needed for later reattachment.

A crash sees all of that or none of it. `Accepted` cannot mean "queued in an
in-memory channel."

A caller-supplied already-signed event is cryptographically verified at the
engine acceptance boundary before any pending state, journal row, or `Accepted`
fact exists. A forged event returns a typed acceptance failure and never reaches
a relay. No accepted obligation or pending event row is committed.

## The pending row is the optimistic UI

Acceptance inserts the draft through the ordinary store door:

```text
StoredRow {
  eventId: final id,
  body: frozen body,
  provenance.local: intentId,
  signature: Pending
}
```

The row immediately participates in ordinary filtering. If it is the current
replaceable/addressable winner, matching live queries see it immediately.
Derived bindings, winner selection, deletes, expiry, GC claims, and query
invalidation use the same row path as relay-observed events.

There is no direct write-to-observer callback and no app-side optimistic mirror.

Because the signature is not part of a NIP-01 event id, signer success promotes
the same row:

```text
Pending -> Signed(signature)
```

Before promotion, NMP verifies that the signer response matches the frozen body,
expected pubkey, and id and carries a valid signature.

## Provider unavailability is a durable state

If the selected signer is unavailable, the row remains visible and the receipt
reports `awaitingSigner(pubkey)` among the facts the receipt stream delivers.

A configured provider being unavailable is not terminal failure. The obligation
survives until that provider becomes available, the app cancels it, protocol
expiry makes it invalid, or a terminal signer/protocol response occurs.

NMP persists the obligation and identity reference, never raw secret material.

## Cancellation and replaceable compensation

Explicit cancellation or terminal pre-signature failure removes the pending row
through the ordinary store door. If it provisionally displaced a replaceable
winner, that previous row is offered back through the same insertion logic.

The supported engine exposes cancellation by stable
receipt id. It is legal only while the accepted write is still unsigned:
successful cancellation returns `WriteCancellationOutcome.cancelled` and
persists the matching receipt fact, repeating it is idempotent, and unknown,
already-signed, already-compensated, or abandoned receipts produce distinct
typed refusals. A persistence failure leaves the obligation live and cannot
emit a false terminal fact. Dropping a receipt observer alone never cancels the
write.

There is no special "un-supersede" API.

Once a valid signature promotes the row, relay ACK, rejection, timeout, and
retry outcomes change receipt evidence only. They never retract the signed row
or resurrect its predecessor.

## Receipt facts

Illustrative facts include:

```text
accepted(intentId, retention)
awaitingSigner(pubkey)
signed(eventId)
routeAdded(relay, reason)
attemptStarted(relay, ordinal)
sent(relay, ordinal)
acked(relay, message?)
rejected(relay, reason)
retryEligible(relay, at)
gaveUp(relay, reason)
outcomeUnknown(relay)
cancelled
failed(reason)
```

The engine reports observations and durable policy state. It does not collapse
them into `published = true` or claim convergence over unknowable relays.

Durable receipt history remains addressable after the delivery obligation is
terminal. Recovery of open work and retention of terminal receipt facts are
separate concerns; closing a delivery lane must not erase the only reattachment
record.

An app that needs the final publication answer calls `Receipt.result()`.
That result contains the whole-write outcome and the final state of every
known relay; mixed publication and rejection remain mixed, including the
rejecting relay's reason. Raw receipt facts remain available for progressive
UI, but terminal reduction, lag recovery, and retained-page traversal belong
to NMP.

## Durability classes

### Durable

NMP retains the obligation across restart until explicit cancellation, terminal
signer/protocol failure, protocol expiry, or the required relay lanes become
terminal under policy. Temporary signer, relay, AUTH, and network unavailability
do not silently close it.

### Explicitly non-durable

The app declares that delay makes the operation worthless. NMP may keep it only
for the current process/attempt and does not resume the publication obligation
after process loss.

It still has a receipt stream and a reattachable minimal receipt record. Its
acceptance fact carries the weaker retention scope, and verification, routing,
and relay failures remain observable. If the process ends before a terminal
handoff fact, reattachment reports an explicit policy-abandoned terminal rather
than retrying or silently forgetting the write. Non-durable does not mean
silent fire-and-forget.

### At most once

NMP persists enough handoff evidence to avoid a blind resend. If a crash or
connection loss makes the outcome unknowable after dispatch, the lane becomes
`outcomeUnknown`. It is never retried as though no attempt happened.

The names may change. These distinctions may not collapse.

## Retry ownership

| Domain | Single owner |
|---|---|
| Socket connection | transport reconnects the socket |
| One remote signing request | signer adapter owns correlation and its connection/AUTH |
| One `(intent, relay)` delivery lane | publish queue owns attempts and eligibility |
| Time and concurrency | one engine deadline scheduler wakes eligible work |

Transport does not hide durable EVENT frames in an independent buffer. The
delivery persists `attemptStarted` before dispatch, exact signed bytes, ordinal,
outcome, and next eligibility. Restart resumes from those facts without polling.

Offline or AUTH-blocked time does not consume an attempt. Route discovery may
append a new relay lane without erasing prior evidence or reopening completed
lanes.

## Protocol-aware publication

An opt-in module can construct a typed operation while preserving the same
receipt plane: a group's own `publish` call takes a photo draft and a
durability policy and returns the ordinary receipt.

The group contributes only NIP-29 context; the photo module owns the draft
schema; core accepts, signs, stores, routes, and reports one intent.

---

<sub>[Index](README.md) · Related: [Evidence without completeness](11-coverage.md) · [Identity and signers](16-identity.md) · [Replaceable edits](15-editing-replaceable.md)</sub>
