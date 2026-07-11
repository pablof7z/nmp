# Embed NMP in ten minutes

> **Provisional target API.** This is a design preview of the settled v2
> experience, not copy-paste documentation for the current SDK. The example
> uses an app-owned kind so the quickstart does not bless a content model.

We will construct one engine, observe one literal query, publish one durable
draft, and inspect both result streams. There is no NMP app object or provider.

## 1. Define what belongs to the app

The app chooses its protocol and presentation policy:

```swift
enum AppProtocol {
    // This number belongs to the app/protocol, not NMP core.
    static let recordKind: UInt16 = 9_999
}
```

In a real protocol, an opt-in NIP module would expose the kind and typed
builder. Raw kinds remain available for app-owned or experimental protocols.

## 2. Construct one engine

```swift
import NMP

let engine = try NMPEngine(configuration: .init(
    store: .persistent(applicationSupport: "nmp.store"),
    bootstrap: .indexers([
        "wss://purplepag.es",
        "wss://relay.primal.net"
    ])
))

try engine.setCurrentPubkey(currentAccount.pubkey)
try engine.attachSigner(currentAccount.signerProvider,
                        for: currentAccount.pubkey)
```

Bootstrap relays are operator discovery policy. They are not a list that every
query or write is broadcast to. NMP discovers and compiles actual source lanes
from demand and typed protocol facts.

Your app decides where this long-lived value lives: a plain model object,
dependency container you already own, or process service. NMP does not provide
an application container.

The signer is needed only for the write later in this guide. A read-only app
sets a current pubkey only when a binding uses it and need not attach any signer.

## 3. Declare a query value

```swift
let demand = NMPDemand(
    selection: NMPFilter(
        kinds: .literal([AppProtocol.recordKind]),
        authors: .literal([selectedAuthor])
    ),
    source: .authorOutboxes,
    access: .public
)
```

This query is deliberately boring. It proves the primitive path without
smuggling a feed, follows list, profile convention, or favored kind into core.

The three descriptor dimensions matter:

- `selection` decides which canonical rows match;
- `source` authorizes NIP-65 author-outbox discovery; and
- `access` says the request is public rather than AUTH-scoped.

## 4. Observe native snapshots

```swift
for await snapshot in try engine.observe(demand) {
    rows = snapshot.rows

    for source in snapshot.acquisition {
        renderSourceFact(source)
    }

    if !snapshot.shortfall.isEmpty {
        renderLocalLimits(snapshot.shortfall)
    }
}
```

The first snapshot may contain cached rows before any socket connects. Later
snapshots update the same local view as sources connect, require AUTH, reach
EOSE, reconcile a watermark, disconnect, or hit a local limit.

There is no `syncHealth` or global `complete` flag. The app interprets scoped
facts for its own UX.

In SwiftUI, the loop belongs in the view/model task you already use:

```swift
.task(id: demand) {
    for await snapshot in try engine.observe(demand) {
        model.apply(snapshot)
    }
}
```

Cancellation and ARC release the observation. The app never sends `CLOSE` or
reopens a Nostr `REQ` itself.

## 5. Publish an immutable draft

```swift
let draft = NMPDraft(
    kind: AppProtocol.recordKind,
    tags: [],
    content: encodedRecord
)

let receipt = try engine.publish(.init(
    draft: draft,
    durability: .durable
))

for await fact in receipt.facts {
    renderWriteFact(fact)
}
```

The signer registered for `$currentPubkey` is the default. The app can override
it for one operation:

```swift
let receipt = try engine.publish(.init(
    draft: draft,
    durability: .durable,
    signer: .identity(podcastIdentity)
))
```

That override does not change the current pubkey.

## 6. Understand the immediate local result

After durable `accepted(intentId)`, any matching query sees the canonical local
row through the store's normal invalidation path:

```swift
switch row.signatureState {
case .pending(let intentId):
    renderPending(intentId)
case .signed:
    renderPublished()
}
```

There is no app-maintained optimistic copy. When a signer arrives, the same row
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

```swift
for await diagnostics in engine.observeDiagnostics() {
    diagnosticsModel.apply(diagnostics)
}
```

Render the current source plan, exact wire filters, connection/AUTH state,
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
