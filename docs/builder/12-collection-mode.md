# Feeds & the Collection observation mode

**Status: PLANNED** — this is the *intended* shape, not yet shipped. It is a provisional design (VISION §10), Tier-A at the M4 SDK boundary; it does not touch the M1 demand resolver. Every code block below is a design preview: the names and shapes may shift at ratification. Where you need feeds *today*, use a plain `observe` and sort/window in app code (see *Consuming results*), and read this chapter to understand where that pattern is headed.

After this chapter you will know why NMP will grow an *ordered, windowed* way to observe a query — a feed — without adding a third noun, and exactly which parts of "a feed" the engine will own versus which stay yours.

## The problem it solves

*Consuming results* ends on a cliff: a plain `observe` re-delivers its entire accumulated row set on every ingest — O(rows²) over a session, measured at ~3.35M deliveries for ~2,587 notes. That is fine for a bounded profile query and ruinous for an infinite home feed. A feed also needs three things a raw `observe` doesn't give you: a stable **order**, a bounded **window** (you don't hold 50,000 rows in memory to show 20), and **pagination** (scroll up, load older). The Collection observation mode is the engine-side answer to all four at once.

## It is a mode of the read noun, not a third noun

This is the load-bearing design decision, so hold it firmly: **Collection is not a new primitive.** It is an opt-in *observation mode* of the live query you already have. A collection observation hangs off the **same demand key and the same graph node** a plain `observe` of that filter would use — it shares the replica, the fan-out, the coverage. What it adds sits entirely on the *result* side: ordering + a bounded window + pagination, layered over the same node. A plain query yields row deltas; a collection yields an ordered, windowed view of the same rows. Nothing about demand, routing, or the binding grammar changes.

The intended Swift shape:

```swift
// PLANNED — design preview, names provisional.
let feed = try engine.observeCollection(
    FeedFilters.follows(kinds: [1]),
    order: .createdAtDescending,          // a closed OrderKey — see below
    window: .init(limit: 50)              // bounded: the engine holds a window, not the world
)

for await view in feed {                  // view: an ordered, windowed CollectionView
    render(view.rows)                     // already ordered; only the window is materialized
    hasMore = view.hasMore                // derived from coverage, not a guess
}
```

## Pagination is widen-only, and `has_more` is just coverage

Two subtleties that keep Collection honest against the bug-ledger.

**`loadMore` is not a second demand mechanism.** Paging older content does exactly one thing: it *widens the query's own `since`/`until`/`limit`*. It never opens a parallel subscription, never spins up a separate cursor machine — it grows the node's own window, consistent with the widen-only rule the rest of the engine follows.

```swift
// PLANNED — loadMore widens the SAME node's since/until/limit.
await feed.loadMore()     // e.g. lowers `since` / raises `limit` on the shared node
```

**`has_more` / `exhausted` / `gap` are not new concepts** — they are *Coverage: empty vs unknown* surfacing under a feed-friendly name. "Is there older content to load?" is answered from the same `CompleteUpTo(watermark)` vs `Unknown` variant you already know: if the window's lower edge is proven complete, the feed is `exhausted`; if it's `Unknown`, there may be more (`has_more`); a proven hole between covered regions is a `gap`. You are not learning a new trust model; you are reading the coverage model through the feed's window. Everything from the coverage chapter applies unchanged.

## Heterogeneous feeds: a *list* of queries, merged result-side

A real home feed is not one filter — it's notes *and* reposts *and* long-form, from several shapes at once. Collection handles this by taking a **list** of queries and merging their delivered rows result-side, deduped by a canonical row key:

```swift
// PLANNED — a composite feed from several queries, merged and ordered by the engine.
let home = try engine.observeCollection(
    [notesFromFollows, repostsFromFollows, longformFromFollows],
    order: .createdAtDescending,
    window: .init(limit: 50)
)
```

Each member query is a normal live query with its own demand node; the collection unions their rows, dedups by `RowKey`, and presents one ordered window. Merging is a *result-side* operation — it never entangles the demand graphs of the member queries.

## Closed `OrderKey` and `RowKey` — and the open design fork

Ordering and row identity are the engine's to compute, because it keys, sorts, and windows on them — so, exactly like `Selector` on the demand side, they are **closed vocabularies**, never app comparators or closures:

- **`OrderKey`** — the closed set of orderings the engine can maintain (e.g. `createdAtDescending`, `createdAtAscending`; the exact members are the Tier-A question). Deterministic order is a property the engine *proves*, which it cannot do over an opaque app comparator.
- **`RowKey`** — the closed rule for canonical row identity (defaulting to `event.id`; addressable coordinates and other shapes are the design question), used to dedup the merge.

This preserves *values in, code after* on the feed surface: the engine orders and keys off introspectable values, and your app code still folds the *delivered, ordered* rows into view state freely.

**The honest open fork.** There is real tension here, and the manual won't paper over it. Apps legitimately want app-defined sort — "rank by my own WoT score," "boost posts from close friends," "my algorithm." A closed `OrderKey` cannot express that, *by design*, because an app comparator feeding the engine's cursor is precisely the bug that was rejected at the Collection gate: the forwarded feed-framework design let an app-computed sort key feed engine *cursor correctness*, and that is the v1 feed framework reborn (VISION §10; candidate bug-ledger #13, two-cursor separation). So the fork is:

- **Engine-side ordering** stays a closed `OrderKey` — deterministic, introspectable, cursor-safe — and app-defined ranking lives *after* delivery, as a **delivery-side transform** (a custom sort over already-delivered rows; see *Delivery-side transforms*). The cost: a post-delivery sort can only reorder the rows already in the window, so it composes awkwardly with engine-side pagination (you can't page by a key the engine doesn't hold).
- The unresolved question the Tier-A round must answer is whether that's *sufficient* — whether every real app-sort need decomposes into "closed OrderKey for the window boundary + delivery-side re-sort within it," or whether some genuinely need the engine to page by an app-supplied key (which would demand the two-cursor separation of ledger #13 be built, not just the closed vocabulary). This is not settled. Build against the closed `OrderKey` + delivery-side sort model for now; treat engine-paged app-sort as an open design item, not a promise.

## Virtualization stays yours

One boundary this mode does *not* cross: **NMP owns the virtualizable collection; your UI framework owns the virtualization.** The engine gives you stable row ids, deterministic order, a bounded window, deltas, and coverage. Which of the visible rows to actually instantiate — that is SwiftUI's `List`, Compose's `LazyColumn`, Dioxus's virtual list. The engine hands you a well-ordered, windowed, keyed collection; the platform decides what to render. NMP owns the collection, not the virtualization.

## What to do today

Until this ships, build feeds the way *Consuming results* shows: a plain `observe`, sort in the `for await` loop, and cap the query with `limit:` so the accumulated set can't grow unbounded. Keep your sort and window logic in one place, framed as "app render policy over delivered rows" — that is exactly the seam that will slot onto `OrderKey`/`window` when Collection lands, so nothing you write now is wasted. And keep reading coverage: `has_more` is just `CompleteUpTo` vs `Unknown` wearing a feed's clothes.

---

<!-- nav-footer -->
<sub>← [Coverage: empty vs unknown](11-coverage.md) · [Index](README.md) · [Delivery-side transforms](13-delivery-transforms.md) →</sub>
