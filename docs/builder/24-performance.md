# Cost & performance: the pay-as-you-go mental model

**Status: BUILT** (the refcounted-demand and delta-accumulation behavior is real — anchored to `nmp-router/src/coalesce.rs`, the Swift `RowBridge` in `Packages/NMP/Sources/NMP/Query.swift`, and the `Handle` in `nmp-engine/src/runtime/mod.rs`. The modularity/feature-flag mechanism is PLANNED-shape and marked as such.)

After this chapter you'll have a concrete cost model for NMP: what a query actually costs, why two identical queries cost the same as one, what happens when the last observer of a query goes away, and the one delta-batching cost you must manage yourself for very large feeds. You'll also see how the modularity principle keeps your *binary* small — you link only the protocol modules you enable.

## The mental model: pay as you go

The design promise is "two calls for a small app, twenty for a full client; zero architecture either way." The cost model matches that shape: **you pay for the demand you declare, and nothing else.** There is no ambient sync, no background firehose, no fixed per-app tax. An app observing one query costs one query's worth of routing and wire subscriptions. An app observing none costs a store and an idle engine thread.

Concretely, a query's cost has three parts:

1. **Wire subscriptions** — the `REQ`s the engine opens on relays to satisfy your demand. You can see exactly how many, and to whom, in the diagnostics `wireSubCount` and `filters` (see *[Diagnostics & debugging](22-diagnostics.md)*). This is the real network cost.
2. **Resolution/re-eval** — for a `Derived` or `Reactive` binding, the engine re-evaluates the binding graph when its inputs change (a new kind:3, an account switch) and surgically re-routes: it closes exactly the departed authors and opens exactly the new ones, with zero churn on the unchanged. You pay for *changes*, not for standing still.
3. **Delivery** — accumulating deltas and handing you snapshots. Cheap per event; the one place it can bite at scale is covered below.

## Identical demand is shared: refcounted queries

The single most important cost fact: **identical queries share.** Demand is refcounted, keyed on the query descriptor. If three views each `observe` the same filter, the engine does not open three sets of wire subscriptions — it opens one, and fans the delivered rows out to all three observers. The wire cost is paid once.

This goes further than exact-match sharing. The router runs a *widen-only* coalescer: two filters identical in every field except `authors` merge into a single `REQ` for the union of both author sets (the `AuthorUnion` rule), because a superset filter matches strictly more events. So a query for "Alice's notes" and a query for "Bob's notes" can collapse into one wire subscription for `authors:[Alice, Bob]`, with the engine demultiplexing the results back to the right observers. The correctness contract is explicit — a merge is only allowed if it provably widens (`matches(merge(a,b)) ⊇ matches(a) ∪ matches(b)`); a rule not proven to widen is dropped and its filters ship separately. Exact-canonical dedup is the trivially-correct floor beneath all of it.

The practical consequence for you: **don't hand-optimize by trying to share query objects across your views.** Declare the demand each view honestly needs. If two views need the same thing, the engine already coalesces them on the wire — you get the sharing for free, keyed on the value, without threading a shared handle through your app. Values-in means the engine can hash, dedup, and coalesce; that's a benefit you'd forfeit by passing around opaque shared subscriptions.

## Last-observer-drop: teardown is refcount-driven

The mirror image of sharing: when the *last* observer of a demand goes away, the underlying wire subscription is torn down. Teardown rides ownership — deinit/ARC in Swift, `Drop` in Rust, flow-collection scope in Kotlin. When your `NMPQuery` (or its iterator) is released, the Rust side's `Drop` unsubscribes automatically; when the query was the last one contributing that demand, the `REQ` closes on the wire. No `cancel()` call is required (though it exists for explicit early teardown).

So the cost of a screen you navigate away from is reclaimed automatically, as long as you let the handle go out of scope. The engine also applies a teardown-with-grace debounce, so briefly dropping and re-adding the same demand (a view reappearing) doesn't thrash the wire. The lever you own is *how long you keep handles alive* — the same lever as lifecycle (see *[Threading, the main-thread contract & app lifecycle](23-threading-lifecycle.md)*).

## The one cost you manage: a full snapshot per delta

Here is the honest sharp edge. The Swift `RowBridge` accumulates row deltas into a full snapshot and yields the *entire current row set* on every batch:

```swift
// Packages/NMP/Sources/NMP/Query.swift (real)
func onBatch(deltas: [FfiRowDelta], coverage: FfiCoverage) {
    lock.lock()
    for delta in deltas { /* apply Added/Removed to byId + order */ }
    let snapshot = order.compactMap { byId[$0] }   // full list, every time
    lock.unlock()
    continuation.yield(RowBatch(rows: snapshot, coverage: Coverage(coverage)))
}
```

This is deliberate — each element you receive is a complete, self-consistent snapshot, so you never reconstruct state from partial deltas and never see a torn read. For a feed of tens or low hundreds of rows, this is a non-issue: rebuilding a small array per batch is trivial.

But for a **large feed** — thousands of rows, arriving in a burst during initial sync — yielding a full snapshot per delta means your consumer re-processes the whole array on every yield. If your `for await` loop does an O(n) sort and a full SwiftUI diff each time, you can spend real CPU on redundant work while a burst lands.

The mitigation is **batch/coalesce on your side**, and you already have the tools:

```swift
// Coalesce bursts: only act on the latest snapshot per animation frame.
var latest: RowBatch?
for await batch in query {
    latest = batch
    // Debounce: yield to the runloop, then take only the freshest.
    await Task.yield()
    guard let b = latest else { continue }
    latest = nil
    rows = b.rows.sorted { $0.createdAt > $1.createdAt }
    coverage = b.coverage
}
```

Because each batch is already the full picture, **dropping intermediate batches is always safe** — the next one supersedes it entirely. That's exactly what makes coalescing correct here: you can throw away every snapshot but the last one in a burst and lose nothing. For the Collection observation mode (PLANNED — *[Feeds & the Collection observation mode](12-collection-mode.md)*), the engine will additionally own bounded windows and stable ordering, moving this work behind the boundary; until then, coalesce large feeds in your consumer.

Diagnostics needs none of this — its stream is already latest-wins at the source, dropping stale snapshots for you.

## Modularity: you link only what you enable

Cost isn't only runtime — it's *binary weight*, and NMP's modularity principle governs it. The engine core is the two nouns plus the hard concerns (store, routing/outbox, sync, coverage, identity, diagnostics, capability seams). **Everything protocol-specific and non-primitive — reactions, reposts, follow packs, highlights, long-form, lists — is opt-in and modular.** A minimal app that never reacts links *zero* reaction code; adding follow-pack support must not tax every other app.

```swift
// PLANNED-shape: you pay for the NIPs you enable.
// Enable a protocol module → its recipes and kinds appear:
import NMPReactions        // now .reaction(to:) and .reactions(to:) exist
// Don't import it → that code isn't in your binary at all.
```

The mechanism (per-NIP crate, Cargo `feature` flag, or registerable module) is being finalized; the *principle* is durable and load-bearing: **you pay only for the NIPs you enable.** This is the old NMP's genuine win, kept — apps that didn't care about reactions never packed `.react()`. When you package per platform (*[Packaging, build & distribution](08-packaging.md)*), you compose only the modules you enabled, and that composition is what determines binary size.

## The summary cost table

| You do | You pay |
|---|---|
| Observe a query | one coalesced set of wire subs (shared with any identical/mergeable demand) |
| Observe the same query twice | nothing extra — refcounted, one wire cost |
| Let a query handle go out of scope | teardown, automatically; wire sub closes when it was the last observer |
| Observe a huge feed during a sync burst | full-snapshot-per-delta CPU — coalesce on your side (dropping intermediate batches is safe) |
| Never use reaction/repost/etc. | zero bytes for those NIPs (PLANNED modularity) |
| Background the app | nothing — demand survives; foreground replays |

The through-line: declare honest demand as values, let the engine share and reclaim it, and the only thing left for you to tune is coalescing very large delivery bursts — which is safe precisely because every batch is already the whole truth.

---

<!-- nav-footer -->
<sub>← [Threading & lifecycle](23-threading-lifecycle.md) · [Index](README.md) · [Testing](25-testing.md) →</sub>
