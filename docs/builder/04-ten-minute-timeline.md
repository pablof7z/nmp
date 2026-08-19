# Embed NMP in ten minutes

> **Provisional target API.** This is a design preview of the settled v2
> experience, not copy-paste documentation for the current SDK. The example
> uses an app-owned kind so the quickstart does not bless a content model.

We will construct one engine, observe one literal query, publish one durable
draft, and inspect both result streams. There is no NMP app object or provider.

## 1. Define what belongs to the app

The app chooses its protocol and presentation policy: an app-owned constant
naming its record kind (say, 9999) belongs to the app/protocol, not to NMP
core.

In a real protocol, an opt-in NIP module would expose the kind and typed
builder. Raw kinds remain available for app-owned or experimental protocols.

## 2. Construct one engine

Constructing the engine takes a persistent store location and a set of
bootstrap indexer relays. Adding a private-key account and marking it current
follows.

Bootstrap relays are operator discovery policy. They are not a list that every
query or write is broadcast to. NMP discovers and compiles actual source lanes
from demand and typed protocol facts.

Your app decides where this long-lived value lives: a plain model object,
dependency container you already own, or process service. NMP does not provide
an application container.

The private-key-backed account is needed only for the write later in this
guide. A read-only app adds a decoded public-key-only account to the session
and marks it current when a binding needs current selection; signing is
unsupported for that account rather than temporarily unavailable.

## 3. Declare a query value

A demand pairs a selection — the app's record kind and a literal author — with
public access. This query is deliberately boring. It proves the primitive path without
smuggling a feed, follows list, profile convention, or favored kind into core.

The three descriptor dimensions matter:

- `selection` decides which canonical rows match;
- read routing is unset here, which is the app saying nothing and NMP
  routing it; and
- `access` says the request is public rather than AUTH-scoped.

## 4. Observe native snapshots

Observing the demand delivers snapshots: rows to render, per-source
acquisition facts to render, and any shortfall to surface as a local-limit
notice.

The first snapshot may contain cached rows before any socket connects. Later
snapshots update the same local view as sources connect, require AUTH, reach
EOSE, reconcile a watermark, disconnect, or hit a local limit.

There is no `syncHealth` or global `complete` flag. The app interprets scoped
facts for its own UX.

The observation loop belongs in the view/model task or scope you already use.

Cancellation releases the observation. The app never sends `CLOSE` or
reopens a Nostr `REQ` itself.

## 5. Publish an immutable draft

A draft carries the app's record kind, tags, and content. Publishing it
durably returns a receipt whose facts are rendered as they arrive.

The signer registered for the current pubkey is the default. The app can
override it for one operation by naming an explicit identity on the publish
call; that override does not change the current pubkey.

## 6. Understand the immediate local result

After durable `accepted(intentId)`, any matching query sees the canonical local
row through the store's normal invalidation path: its signature state is
either pending or signed, and the app renders each state accordingly.

There is no app-maintained optimistic copy. When the configured provider
becomes available, the same row
is promoted because a NIP-01 event id does not include its signature.

The receipt may then report facts such as:

```text
accepted(intentId)
awaitingSigner(pubkey)
signed(eventId)
routed(relay)
attemptStarted(relay, ordinal)
acked(relay)
rejected(relay, reason)
outcomeUnknown(relay)
```

Those are observations, not a single success boolean.

## 7. Keep diagnostics permanent

Diagnostics is its own permanent observation stream. Render the current source plan, exact wire filters, connection/AUTH state,
events received by relay and kind, coverage watermarks, limits, and write
attempts. That screen is the proof surface for machinery the app deliberately
does not own.

## What you did not write

- a relay pool or subscription manager;
- a watcher that reopens requests when dependencies change;
- an optimistic row overlay;
- a signer retry loop;
- a transport-owned durable publish buffer;
- an NMP provider, reducer, or scene-phase hook; or
- a global-sync interpretation.

That absence is the product.

---

<sub>[Index](README.md) · Related: [Mental model](02-mental-model.md) · [Ownership reference](05-two-nouns.md) · [Binding grammar](09-binding-grammar.md)</sub>
