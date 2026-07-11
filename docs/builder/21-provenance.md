# Provenance and private routing authority

Provenance records where a row came from and why a source/relay appears in a
plan. It must survive dedup, coalescing, persistence, and protocol composition.

## Stored-row provenance

When the same verified event arrives from several relays, NMP stores one
canonical row and merges every observation into its provenance:

```text
StoredRow {
  event,
  provenance: {
    local: intentId?,
    seen: { relay -> observedAt }
  }
}
```

No query path returns a row whose provenance was discarded by id dedup. A local
pending row keeps its local origin when relay echoes later grow `seen`.

## Route provenance

Every source/wire lane records a typed reason:

- author outbox discovered through validated NIP-65 facts;
- protocol host minted from a validated protocol reference;
- private inbox derived from verified recipient state;
- operator discovery policy; or
- another closed authority accepted by the compiler.

Diagnostics connects that reason to the descriptor/intent, access context,
exact relay/filter, and any coalescing decision.

## A no-widen set is not authority

An internal set type with no `insert`/`union` method is useful after private
destinations have been validated. It prevents accidental widening during later
planning.

It is not sufficient if app code can construct the initial set from arbitrary
URLs. A public `NarrowOnly::new(relays)` would merely make an arbitrary relay
set immutable; it would not prove that those relays are recipient inboxes.

Private authority must therefore be minted by the owning protocol module or
engine from verified facts:

```swift
let route = try Nip17.recipientRoute(
    recipients: recipients,
    using: engine
)

let receipt = try engine.publish(.init(
    draft: giftWrapDraft,
    durability: .durable,
    context: route
))
```

The app cannot inspect or widen `route` into public relays. If inbox facts are
missing, the operation fails closed with explicit receipt/shortfall evidence.

## Received private data is not a publication capability

A private row's event bytes and provenance do not authorize republishing it via
an author-outbox or fallback route. Re-publication must go through an explicit
operation owned by the private protocol, which validates recipients/context and
mints a new permitted route (and, where required, a new encrypted wrapper).

There is no conversion from opaque private authority to public routing.

## Coalescing preserves attribution

When compatible source requests share one widened wire filter, NMP retains the
descriptors, authority, access contexts, and coverage keys absorbed into the
request. Local re-filtering delivers rows only to valid selections, and one
source's evidence cannot prove another access context.

## What to verify in diagnostics

- one canonical row retains every relay observation and local origin;
- every private lane names the protocol authority that minted it;
- no public/fallback lane appears for a private operation;
- an empty recipient route fails before `PublishEvent`;
- AUTH/source evidence remains attached to its context; and
- coalescing never drops descriptor or coverage attribution.

These facts make privacy/routing claims inspectable, but the real guarantee is
the absence of a public constructor that can forge authority.

---

<sub>[Index](README.md) · Related: [Capabilities](20-capabilities.md) · [Source and routing context](17-relays.md) · [Diagnostics](22-diagnostics.md)</sub>
