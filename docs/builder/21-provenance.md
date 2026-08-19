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

Diagnostics connects that reason to the descriptor/intent, authenticated identity,
exact relay/filter, and any coalescing decision.

## Naming an exact relay set is a capability, not an authority

An app may say "use these exact relays and that is that". That is
`WriteRouting.explicit`, and it is deliberately general: a user typing a
relay into a text field, a wiki module publishing to the user's preferred
wiki relays, and a DM module publishing to two parties' inboxes are the same
primitive. Guarding it behind a mintable authority newtype was tried and
rejected — there are many legitimate reasons to publish to a specific relay,
and NIP-29 is one consumer of the capability rather than its justification.

What the route DOES guarantee is structural, and it is a routing property
rather than a privacy claim:

- it executes verbatim — the relay directory is never consulted, so nothing
  it knows or later learns can contribute a relay;
- nothing widens it after acceptance; and
- an empty set is refused at the door, never resolved to nothing and never
  downgraded to `auto`.

What it does NOT guarantee is that the relays mean anything in particular. A
module that needs "these relays are this recipient's verified inboxes" owns
that validation itself and returns one ordinary `WriteIntent` containing an
`EventBuilder` plus the exact route minted from those verified facts. NMP does
not currently ship a NIP-17 composer, so this document deliberately does not
invent a call shape for one. The old `.unsigned(giftWrapDraft)` example
was removed with `WritePayload::Unsigned`; the current generic builder payload
is `WritePayload::Event(EventBuilder)`, and a future semantic NIP-17 door
must own its composition rather than asking presentation code to fill those
fields.

If inbox facts are missing, that operation fails before it publishes, with
explicit evidence — the app does not send an empty route and read the
failure off a receipt.

## Received private data is not a publication capability

A private row's event bytes and provenance do not authorize republishing it via
an author-outbox or fallback route. Re-publication must go through an explicit
operation owned by the private protocol, which validates recipients/context and
mints a new permitted route (and, where required, a new encrypted wrapper).

There is no conversion from opaque private authority to public routing.

## Coalescing preserves attribution

When compatible source requests share one widened wire filter, NMP retains the
descriptors, authority, authenticated identities, and coverage keys absorbed into the
request. Local re-filtering delivers rows only to valid selections, and one
source's evidence cannot prove another authenticated identity.

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
