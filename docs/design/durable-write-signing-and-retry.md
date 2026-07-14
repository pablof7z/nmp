# Durable writes, signing, and retry

- **Status:** IMPLEMENTED - crash-safe acceptance, canonical pending rows,
  signer reattachment, the one durable retry scheduler, and truthful governed
  lane-state projection across Rust/FFI/Swift/Kotlin satisfy this contract.
- **Owns:** the meaning of `Accepted`, pending-row semantics, signer selection,
  receipt persistence, and retry ownership.

## 1. Acceptance transaction

For a durable write, `Accepted` is emitted only after one atomic persistence
boundary records:

- the frozen unsigned NIP-01 body and expected author pubkey;
- the stable event id derived from that body;
- the durable intent and receipt identity;
- signature state `Pending(intentId)`;
- the canonical pending row inserted through the ordinary event-store mutation
  path;
- any displaced replaceable winner needed for pre-signature compensation;
- initial route/retry state that is already known.

If that transaction fails, the caller receives an acceptance error and no
pending row becomes visible. `Accepted` never means merely queued in memory.

### Guarded whole-value replacement

A protocol module composing a destructive replaceable/addressable edit may
attach the exact canonical base event id it observed. `None` means the module
established no local winner under its explicit source-evidence policy; it does
not assert global Nostr absence. NIP-02's ordinary `follow` / `unfollow`
operation deliberately requires `Some(base)`; the generic `None` form does not
silently grant it first-list creation policy.

The store compares that expected base with the current winner inside the same
acceptance transaction, before allocating an intent or receipt id and before
changing the canonical row. A mismatch refuses the acceptance atomically and
surfaces `WriteStatus::ReplaceableConflict { expected, actual }`. It never
silently rebases the draft, and a precondition attached to a regular
non-replaceable event fails closed.

This mechanism closes the local read/accept race. It does not turn EOSE or a
watermark into global completeness: the protocol operation separately owns
which planned sources and evidence are sufficient to compose at all. Raw FFI
writes cannot mint the guard; native callers reach it through semantic
operations such as NMP's NIP-02 `follow` / `unfollow` action.

## 2. One row path

The pending row participates in ordinary filters, derived bindings,
replaceable/delete/expiry semantics, persistence, GC claims, and query
invalidation. The write path has no direct observer callback and no optimistic
overlay.

NIP-01 event identity excludes the signature, so the id does not change when a
signature arrives. A valid signature atomically promotes the same row:

```text
Pending(intentId) -> Signed(signature)
```

The returned signed event must match the frozen body and expected pubkey exactly
and must verify cryptographically before promotion.

Cancellation or terminal pre-signature protocol failure removes the pending row
through the ordinary store door. If it displaced a replaceable winner, the
engine offers that prior row back through the same door as a compensating
mutation. After signature promotion, relay ACK/rejection changes receipt state
only; it never retracts the valid signed event.

## 3. Signer selection and reattachment

The ergonomic default is the signer registered for `$currentPubkey`:

```text
publish(draft)
publish(draft, as: identityRef)  // exceptional override
```

The override supports podcast identities, disposable identities, delegation,
hardware keys, and similar cases without making them globally active. The app
does not need to retain or pass a signer object on ordinary writes.

Before acceptance NMP resolves a stable expected author identity. At acceptance
that identity is pinned. A later current-pubkey change cannot redirect the
intent to another signer.

If the matching capability is absent or temporarily offline, the receipt says
`AwaitingSigner(pubkey)`. The durable obligation remains until the app attaches
a matching signer, explicitly cancels it, a terminal protocol failure occurs,
or protocol expiry makes it invalid. Missing NIP-46 connectivity is not failure.

### Governed sign-only operation

Signing and publishing are orthogonal. A host that must authorize an external
client's exact Nostr event uses the engine's sign-only operation rather than
fabricating an ephemeral write intent.

The request carries an immutable unsigned NIP-01 body whose author must equal
the active account. Acceptance freezes that author and body, resolves only the
matching registered capability, and admits pending signer work through the
same finite native-task owner used by other signer requests. The returned event
is released only after its body, author, computed id, and signature all
validate. Cancellation is scoped to that one signer operation.

This path deliberately bypasses write acceptance. It creates no canonical
pending row, intent or receipt id, outbox journal/lane, relay plan, or
publication. NIP-07 origin authorization and prompting remain host policy; the
operation supplies governed key custody and exact-result validation only.

## 4. Secret-material boundary

The Rust event/outbox store persists signing obligations, expected pubkeys,
frozen bodies, and validated signatures. It does not persist raw secret keys.

Platform SDKs should ship standard signer providers backed by platform secure
storage so ordinary apps do not hand-roll vault plumbing. The app owns which
identities exist, import/removal/backup UX, and whether to use a custom remote,
hardware, or memory-only signer.

A memory-only disposable key may disappear permanently. Its accepted intent
then remains `AwaitingSigner` until an equivalent signer is attached or the app
cancels it; NMP must not silently discard or re-author it.

## 5. Receipt durability

Receipt facts are persisted and reattachable by intent/receipt id. Dropping an
observer does not cancel the write or lose its history. `Accepted`, signer
waiting, signature promotion, route revisions, attempts, ACKs, rejections,
expiry, cancellation, and ambiguous at-most-once outcomes remain inspectable
after restart.

`Enqueued`, `sent`, and `converged` are never synonyms. Product policy may
interpret a set of per-relay facts; the engine reports them without inventing a
single success boolean.

## 6. Retry ownership

Retry is split by domain, with exactly one owner each:

| Domain | Owner | Durable responsibility |
|---|---|---|
| Socket connection | transport | reconnect the socket; never buffer durable EVENTs invisibly |
| One remote-sign request | signer adapter | correlation, AUTH/connect for that operation, exact response validation |
| One `(intent, relay)` lane | durable outbox | attempt state, eligibility, terminal relay evidence |
| Time and concurrency | engine deadline scheduler | wake eligible work without poll loops or per-intent threads |

For every durable relay lane the outbox persists the exact signed bytes,
`AttemptStarted`, attempt ordinal, outcome, and `nextEligibleAt`. Backoff uses
deterministic jitter and explicit caps so restart does not reset or synchronize
the fleet.

- Offline and AUTH-blocked time do not consume attempts.
- Recovery wakes work whose persisted eligibility time has passed.
- A transient delivery failure advances backoff.
- A relay ACK closes its lane.
- A route revision may add a new lane without reopening completed lanes.
- A permanent relay rejection is terminal evidence for that lane, not row
  retraction.
- At-most-once ambiguity becomes `OutcomeUnknown`; it is never blindly retried.

There is no fixed-rate polling. The scheduler sleeps until the earliest real
deadline and rearms after every state transition.

## 7. Falsification

Required proofs include:

- crash immediately after `Accepted` restores the pending row and receipt;
- matching queries and derived bindings see the pending row through the normal
  store path;
- account/current-pubkey changes cannot change a pinned signer identity;
- signer absence survives restart as `AwaitingSigner` and resumes after attach;
- an invalid or mismatched signer response cannot promote the row;
- pre-signature cancellation restores a displaced replaceable winner;
- an exact-base guarded replacement is accepted, while a concurrent winner
  produces a typed conflict with no intent, receipt, or pending-row residue;
- all relays rejecting a signed event leaves the signed row intact;
- transport reconnect cannot duplicate durable buffering ownership;
- restart preserves attempt ordinal and next eligibility;
- at-most-once ambiguity never emits a second send.
