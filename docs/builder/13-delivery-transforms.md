# Delivery-side transforms: WoT filtering & custom sort

**Status: PLANNED** — this is the *intended* shape, not yet a shipped SDK surface. The underlying rule it formalizes — *app closures may fold delivered rows, never parameterize demand* — is BUILT and load-bearing throughout the read path today; what's PLANNED is a blessed *application point* for those closures. Code blocks are design previews. You can do all of this by hand right now, in your `for await` loop; this chapter names the pattern and where it's headed.

After this chapter you will know the one sanctioned home for arbitrary app code in the read path — a closure that runs over *delivered* rows — and, just as importantly, why that closure is allowed here when the identical-looking thing on the demand side is forbidden.

## The line: demand-side (forbidden) vs delivery-side (allowed)

Every chapter in Part III has repeated one rule: **values in, code after.** Now we cash out the "code after" half.

Draw the boundary at *delivery*. Everything the engine uses to **decide** — what to demand, where to route, how to key a row, how to order a window, where a cursor sits — must be a closed, introspectable *value* (`Binding`, `Selector`, `OrderKey`, `RowKey`). No app closure touches any of it. That is the demand side, and it is closed for a mechanical reason: the engine hashes, dedups, refcounts, coalesces, and routes off those values, and a closure can't be hashed or shared or shown on the diagnostics screen. Put a closure in the demand path and the whole demand-sharing, surgical-delta, cross-account-reroot machine collapses.

But once rows have crossed the boundary — once they are *delivered* to your app — the engine is done deciding. A closure that runs over delivered rows changes nothing the engine routes, keys, or caches. It sits *after* the machine. So it is not only allowed, it is the **sanctioned exception**: the one place arbitrary app logic belongs in the read path. A WoT scorer, a mute heuristic, a custom comparator — welcome, delivery-side; unrepresentable, demand-side.

The whole chapter is that one distinction, applied twice.

## Web-of-trust filtering: an authorless query + a post-facto score filter

Web-of-trust is the motivating case, because the naive design gets the boundary exactly wrong. You want "notes, but only from authors my WoT scores above a threshold." The tempting move is to compute the trusted-author set in app code and hand it to the engine as the query's `authors`. That's a closure (your scorer) parameterizing demand — forbidden, and for good reason: your score function is opaque to the engine, so it couldn't share the node, couldn't reroute surgically, couldn't show you on the diagnostics screen *why* those authors.

The right shape splits cleanly in two:

**1. Demand side — a bounded, authorless-or-broadly-authored query, expressed as values.** You demand notes over a bound the engine *can* reason about — a time window, a set of kinds, your read-relays' natural population — without encoding the score. If your WoT set *is* derivable from Nostr state (follows-of-follows, say), that derivation is itself a `Derived` binding the engine maintains — a value, not your closure (see *Live queries & the binding grammar*). The demand stays introspectable.

**2. Delivery side — your scorer, as a closure over delivered rows.** After delivery, you drop rows whose author scores below your threshold. This is arbitrary app code — any model, any heuristic, any data source — and it is fine *because it runs after the boundary*:

```swift
// PLANNED — a blessed delivery-side application point. The closure sees
// DELIVERED rows; it never parameterizes demand, routing, or cursors.
let feed = try engine.observe(notesFromReadRelays)
    .filter { row in myWoT.score(author: row.pubkey) >= 0.5 }   // app code, post-delivery

// Equivalent you can write TODAY, by hand, in the consume loop:
for await batch in try engine.observe(notesFromReadRelays) {
    let trusted = batch.rows.filter { myWoT.score(author: $0.pubkey) >= 0.5 }
    render(trusted)
}
```

Note what the closure does *not* touch: it doesn't change which relays were queried, doesn't narrow the wire filter, doesn't move a coverage watermark. It thins what you *show*. That's the tell for a legitimate delivery-side transform — remove it and the engine behaves identically; only your rendered set changes.

## Custom sort: the same rule, on ordering

*Feeds & the Collection observation mode* explains that engine-maintained order is a closed `OrderKey` — because the engine keys and paginates on it, and an app comparator feeding the engine's cursor is the rejected v1 feed framework (candidate bug-ledger #13). Custom sort lives on the *other* side of that same line: a comparator over already-delivered, already-windowed rows.

```swift
// PLANNED — a custom comparator over DELIVERED rows. Reorders what the
// engine handed you; never becomes an engine cursor key.
let ranked = try engine.observe(feedQuery)
    .sorted { myRanker.rank($0) > myRanker.rank($1) }

// By hand today:
for await batch in try engine.observe(feedQuery) {
    render(batch.rows.sorted { myRanker.rank($0) > myRanker.rank($1) })
}
```

The falsifier already does the by-hand version — `batch.rows.sorted { $0.createdAt > $1.createdAt }` in its feed view. `createdAt`-descending happens to also be an `OrderKey` the engine could maintain; the *point* is that an app-specific ranking that *isn't* in the vocabulary still has a home — here, delivery-side — instead of forcing a closure into the engine.

**The composition caveat, stated honestly:** a delivery-side sort can only reorder the rows currently in the window. It cannot make the engine *page* by your key, because the engine doesn't hold your key. So app-sort composes cleanly with a plain `observe` (you have the whole accumulated set) and awkwardly with paginated Collection windows (you re-sort within each window, but the window boundary is still a closed `OrderKey`). Whether that's sufficient for every real app-ranking need is the open fork flagged in the Collection chapter — not settled, and not something delivery-side transforms alone resolve.

## Why the SDK gives this a named seam at all

You can already do everything above in your consume loop, so why is a blessed `.filter`/`.sorted`/`.map` application point PLANNED at all? Two reasons, both about keeping the boundary *legible*:

1. **It marks the seam.** A named delivery-side transform API is a place the manual can point to and say "app closures go *here*, and nowhere upstream." That makes the forbidden case (closures in demand) obvious by contrast — the API's shape teaches the rule.
2. **The engine owes it nothing.** Per the design guidelines, the engine's obligation for delivery-side transforms is *nothing beyond delivering rows the transforms can consume*. There is no engine machinery to build — the transform is your code, applied after a boundary that already exists. That's exactly why it's a safe, thin, deletable addition rather than a new primitive.

## What to hold onto

- **Delivery-side = after the boundary = your code is welcome.** Filter, sort, map, score, dedup on your own key — arbitrary app logic over *delivered* rows.
- **Demand-side = before the boundary = values only.** If a transform's *input* is Nostr state, express that input as a `Derived` binding (a value the engine maintains), not as a closure that computes demand.
- **The test for "is this a legitimate delivery-side transform?"**: remove it, and the engine must behave *byte-for-byte identically* — same relays, same wire filters, same coverage. If removing your closure would change what the engine demands or routes, it was never delivery-side, and the API won't let it sit there.
- **Today:** write these transforms by hand in the `for await` loop (*Consuming results*). When the blessed application point ships, your `filter`/`sorted`/`map` slot onto it unchanged.

---

<!-- nav-footer -->
<sub>← [Feeds & the Collection mode](12-collection-mode.md) · [Index](README.md) · [Writing: intents & receipts](14-writing.md) →</sub>
