# Drafts, acceptance, pending rows, and receipts

A write is an intent with an observable receipt, not a call that returns one
success boolean.

## Start from an immutable draft

```swift
let draft = NMPDraft(
    kind: appKind,
    tags: appTags,
    content: encodedContent
)
```

The draft is unsigned. The engine or an enabled protocol module may validate
closed typed context, but no stage mutates an already signed event.

Publishing declares policy:

```swift
let receipt = try engine.publish(.init(
    draft: draft,
    durability: .durable,
    signer: nil,
    context: nil
))
```

- `signer: nil` selects the signer registered for current pubkey.
- an identity override applies to this intent only;
- typed protocol context may contribute route/access facts; and
- the app does not expand ordinary routing into relay arrays.

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
  signatureState: Pending(intentId)
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
Pending(intentId) -> Signed(signature)
```

Before promotion, NMP verifies that the signer response matches the frozen body,
expected pubkey, and id and carries a valid signature.

## Missing signer is a durable state

If the selected signer is unavailable, the row remains visible and the receipt
reports `awaitingSigner(pubkey)`.

```swift
for await fact in receipt.facts {
    switch fact {
    case .awaitingSigner(let pubkey):
        showSignerUnavailable(pubkey)
    default:
        apply(fact)
    }
}
```

A disconnected NIP-46 session is not terminal failure. The obligation survives
until a matching provider reattaches, the app cancels it, protocol expiry makes
it invalid, or a terminal signer/protocol response occurs.

NMP persists the obligation and identity reference, never raw secret material.

## Cancellation and replaceable compensation

Explicit cancellation or terminal pre-signature failure removes the pending row
through the ordinary store door. If it provisionally displaced a replaceable
winner, that previous row is offered back through the same insertion logic.

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
separate concerns; closing an outbox lane must not erase the only reattachment
record.

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
| One `(intent, relay)` delivery lane | durable outbox owns attempts and eligibility |
| Time and concurrency | one engine deadline scheduler wakes eligible work |

Transport does not hide durable EVENT frames in an independent buffer. The
outbox persists `attemptStarted` before dispatch, exact signed bytes, ordinal,
outcome, and next eligibility. Restart resumes from those facts without polling.

Offline or AUTH-blocked time does not consume an attempt. Route discovery may
append a new relay lane without erasing prior evidence or reopening completed
lanes.

## Protocol-aware publication

An opt-in module can construct a typed operation while preserving the same
receipt plane:

```swift
let receipt = try group.publish(photoDraft, durability: .durable)
```

The group contributes only NIP-29 context; the photo module owns the draft
schema; core accepts, signs, stores, routes, and reports one intent.

---

<sub>[Index](README.md) · Related: [Evidence without completeness](11-coverage.md) · [Identity and signers](16-identity.md) · [Replaceable edits](15-editing-replaceable.md)</sub>
