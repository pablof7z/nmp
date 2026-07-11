# Query snapshots and presentation ownership

Observing a demand yields the newest complete **local** state represented by a
snapshot. It does not yield raw relay callbacks and it does not claim global
Nostr completeness.

## Snapshot shape

Illustrative target spelling:

```swift
struct NMPQuerySnapshot {
    let revision: UInt64
    let rows: [NMPRow]
    let cache: CacheEvidence
    let acquisition: [SourceEvidence]
    let shortfall: [Shortfall]
}
```

- `rows` are current canonical store winners matching the selection.
- `cache` identifies the local revision and retained provenance represented.
- `acquisition` reports compact facts for currently planned sources and access
  contexts.
- `shortfall` reports intended work that a source failure, AUTH requirement,
  cap, or local limit prevented.

Exact wire filters, counters, compiler lanes, and history remain in diagnostics.

## Rows are store values, including local pending writes

```swift
struct NMPRow {
    let event: NMPEvent
    let provenance: Provenance
    let signatureState: SignatureState
}

enum SignatureState {
    case pending(intentId: IntentId)
    case signed
}
```

A durable accepted draft appears here through the same store query as a relay-
observed event. The app does not merge a second optimistic collection.

When the signature arrives, the row keeps the same event id and becomes signed.
When a relay echoes it, provenance grows on the same row. A terminal
pre-signature cancellation removes it through normal invalidation.

## Snapshots are latest-state streams

A slow observer may skip intermediate frames. The next frame must contain every
local mutation incorporated through its revision and the evidence/shortfall for
that same revision.

That permits bounded newest-value delivery:

- Swift can frame-coalesce and buffer the newest snapshot;
- Kotlin can expose a conflated cold `Flow`; and
- Rust can use a bounded latest-state receiver/stream.

Skipping an intermediate rendered state is safe. Losing a durable receipt fact
is not; receipts use persistence and reattachment rather than an unbounded
observer queue.

## Fold into app state

```swift
for await snapshot in try engine.observe(demand) {
    model.rows = snapshot.rows
    model.sourceFacts = snapshot.acquisition
    model.shortfall = snapshot.shortfall
}
```

The app may sort, group, rank, filter for presentation, or join rows with
non-Nostr state after delivery:

```swift
model.visibleRows = snapshot.rows
    .filter(productPolicy.admits)
    .sorted(using: productPolicy.order)
```

Those closures see already-delivered rows. They do not parameterize engine
demand, source selection, or cursor correctness.

## Raw event meaning versus protocol modules

Core returns canonical Nostr fields and typed storage metadata. It does not pick
a display name, decode arbitrary content into one universal model, rank posts,
or turn tags into navigation.

An enabled protocol module may parse and validate the exact schema it owns. For
example, a NIP-68 module may project a raw event into a typed photo value. The
app still chooses layout, labels, ordering, and failure presentation.

## Observation lifetime

The native handle owns demand lifetime:

- ending a Swift `for await` loop releases its observation;
- cancelling a Kotlin collector closes its `Flow` bridge; and
- dropping a Rust handle decrements demand.

The engine refcounts shared demand and closes only work no remaining descriptor
requires. The app never mirrors Nostr `REQ` ids or sends `CLOSE` itself.

## Replacing a descriptor

If ordinary app state changes a non-reactive part of the demand, construct a new
value and observe it using the UI/runtime lifecycle you already have:

```swift
.task(id: demand) {
    for await snapshot in try engine.observe(demand) {
        model.apply(snapshot)
    }
}
```

Bindings are for dependencies NMP must own and maintain from Nostr/current-
pubkey state. They are not a requirement to route every app input through an
NMP registry.

---

<sub>[Index](README.md) · Related: [Binding grammar](09-binding-grammar.md) · [Evidence without completeness](11-coverage.md)</sub>
