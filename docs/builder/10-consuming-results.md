# Query snapshots and presentation ownership

Observing a demand yields the newest complete **local** state represented by a
snapshot. It does not yield raw relay callbacks and it does not claim global
Nostr completeness.

## Snapshot shape

Illustrative target shape: a snapshot carries a revision, rows, cache
evidence, acquisition evidence, and shortfall.

- `rows` are current canonical store winners matching the selection.
- `cache` identifies the local revision and retained provenance represented.
- `acquisition` reports compact facts for currently planned sources and access
  contexts.
- `shortfall` reports demand atoms with no covering source and local limits that
  prevented intended acquisition. A planned source's connection or AUTH state
  remains a source-status fact instead.

Exact wire filters, counters, compiler lanes, and history remain in diagnostics.

## Rows are store values, including local pending writes

A row carries the ordinary Nostr fields (id, pubkey, created-at, kind, tags,
content) plus its signature state — pending or signed — and its source
provenance.

A durable accepted draft appears here through the same store query as a relay-
observed event. The app does not merge a second optimistic collection.

When the signature arrives, the row keeps the same event id and becomes signed.
When a relay echoes it, provenance grows on the same row. A terminal
pre-signature cancellation removes it through normal invalidation.

## Snapshots are latest-state streams

A slow observer may skip intermediate frames. The next frame must contain every
local mutation incorporated through its revision and the evidence/shortfall for
that same revision.

That permits bounded newest-value delivery: the facade uses a bounded
latest-state receiver/stream.

Skipping an intermediate rendered state is safe. Losing a durable receipt fact
is not; receipts use persistence and reattachment rather than an unbounded
observer queue.

## Fold into app state

Each delivered snapshot's rows, acquisition evidence, and shortfall fold into
app state.

The app may sort, group, rank, filter for presentation, or join rows with
non-Nostr state after delivery.

Those transforms see already-delivered rows. They do not parameterize engine
demand, source selection, or cursor correctness.

## Raw event meaning versus protocol modules

Core returns canonical Nostr fields and typed storage metadata. It does not pick
a display name, decode arbitrary content into one universal model, rank posts,
or turn tags into navigation.

An enabled protocol module may parse and validate the exact schema it owns. For
example, NIP-22 projects a raw kind:1111 event into a typed comment value. The
app still chooses layout, labels, ordering, and failure presentation.

## Observation lifetime

The native handle owns demand lifetime: dropping a Rust handle decrements
demand.

The engine refcounts shared demand and closes only work no remaining descriptor
requires. The app never mirrors Nostr `REQ` ids or sends `CLOSE` itself.

## Replacing a descriptor

If ordinary app state changes a non-reactive part of the demand, construct a new
value and observe it using the runtime lifecycle you already have.

Bindings are for dependencies NMP must own and maintain from Nostr/current-
pubkey state. They are not a requirement to route every app input through an
NMP registry.

---

<sub>[Index](README.md) · Related: [Binding grammar](09-binding-grammar.md) · [Evidence without completeness](11-coverage.md)</sub>
