# Writing: accepted intent, local state, and relay evidence

**Status: CURRENT + TARGET.** The current engine signs, routes, and streams
per-relay statuses. It does not yet implement the full crash-safe acceptance,
pending cache record, signer reattachment, durable retry, cancellation, or
receipt reattachment described as TARGET below.

## Enqueued is never converged

A write returns a receipt stream because publication has several independently
observable stages. `Accepted` means NMP has taken responsibility for the
declared obligation. It never means a relay accepted the event and never means
the write converged everywhere.

Relay outcomes remain facts, not a single success boolean. An app decides
whether one acknowledgement, several acknowledgements, or a particular relay
is enough for its product.

## What durable `Accepted` must mean

For a durable unsigned write, acceptance is one atomic store transaction that
records:

- the frozen unsigned event body and its expected author;
- a stable intent id and selected signer identity;
- the durable receipt/outbox state;
- the canonical pending event record visible to ordinary matching queries.

If any part cannot be persisted, the call has not accepted the write. There is
no state where the app sees a pending row but the outbox cannot resume it, or an
outbox obligation exists without the row that made the UI reactive.

The app does not wire an optimistic row into a query. Acceptance mutates the
canonical store; normal query invalidation delivers the change to every
matching observer.

## One canonical event record

The accepted record has a typed signature state:

```text
Pending(intentId) | Signed(signature)
```

NIP-01 event id can be computed from the frozen body before a signature exists.
When signing succeeds, the same record is atomically promoted to `Signed`.
Replaceable, delete, and expiry semantics apply through the ordinary store path
in both states.

If a pre-signature write is cancelled or reaches a terminal protocol failure,
the pending record is retracted through an ordinary store mutation. Matching
queries update for the same reason they update after any other store change.

## Signer selection

The normal publish path uses the signer registered for the current pubkey. The
app need not pass a signer object repeatedly:

```text
publish(draft)
```

Exceptional identities use an explicit override:

```text
publish(draft, as: podcastIdentity)
publish(draft, as: disposableIdentity)
```

The selected identity is frozen at acceptance. Changing the current pubkey
later cannot redirect the write.

When the capability is unavailable, the receipt reports
`AwaitingSigner(pubkey)` and the pending row remains visible. Registering a
valid capability later resumes the obligation. NIP-46 being offline is a
temporary condition, not a reason to discard an accepted write.

## Durable receipt facts

The exact public enum remains provisional, but the receipt must distinguish:

- accepted and awaiting signer;
- signed and routed;
- per-relay attempt/sent/ack/rejection evidence;
- temporary blocked/offline/AUTH states;
- next eligible retry where applicable;
- cancellation or terminal protocol failure;
- `OutcomeUnknown` for an at-most-once attempt whose outcome cannot be known.

Receipts are persisted facts keyed by intent id. An app can reattach after
restart; correctness cannot depend on keeping the original in-memory stream
alive.

## Durability classes

- **Durable:** NMP retains the obligation until explicit cancellation, a
  terminal signer/protocol failure, protocol expiry, or the required relay
  lanes acknowledge it. Temporary signer, relay, AUTH, or network
  unavailability does not silently close it.
- **Explicitly non-durable:** suitable for information that becomes worthless
  when delayed. NMP may forget it according to the declared policy, and the
  receipt states that weaker promise explicitly.
- **At-most-once:** NMP never blindly retries an ambiguous attempt. If delivery
  may have happened but cannot be proven, the terminal fact is
  `OutcomeUnknown`, not `GaveUp` followed by a resend.

The exact names may change; these behavioral distinctions may not collapse.

## Retry ownership

There is one owner per retry domain:

- transport reconnects sockets only;
- the NIP-46 adapter owns one correlated signer RPC/AUTH exchange;
- the durable outbox owns persisted per-(intent, relay) delivery attempts;
- one engine deadline scheduler owns timers and concurrency limits.

Transport must not secretly buffer durable EVENT frames. There are no
per-intent threads and no polling loops. Attempt ordinal, exact signed bytes,
outcome, and `nextEligibleAt` are persisted. Offline or AUTH-blocked time does
not consume an attempt; recovery wakes eligible work; transient failure
advances logical backoff; an ACK closes that relay lane.

## Routing and protocol context

Generic public writes use engine-owned routing such as author outbox or inbox
discovery. Apps do not expand those into relay lists.

Some protocols make a relay part of the semantic object. A bound NIP-29 group,
for example, contributes its host relay and `h` tag when publishing a draft.
That typed protocol authority is not a generic app-supplied relay override.

## Current implementation gap

The shipping statuses `accepted`, `awaitingCapability`, `signed`, `routed`,
`sent`, `acked`, `rejected`, `gaveUp`, and `failed` describe the current
in-memory path. In particular, current `Accepted` is not yet the crash-safe
transaction defined above, `gaveUp` currently closes work that durable retry
must retain, and receipts are not reattachable. Builders should treat those as
known implementation gaps, not the final contract.

---

<!-- nav-footer -->
<sub>← [Delivery-side transforms](13-delivery-transforms.md) · [Index](README.md) · [Editing replaceable state safely](15-editing-replaceable.md) →</sub>
