# Consuming results: rows, snapshots, and presentation ownership

**Status: BUILT** — `observe → AsyncSequence`, `Row`, `RowBatch`, and `Coverage` are shipped; the Swift and Rust examples are the real current API.

After this chapter you can take a live query, iterate it, fold each delivery into your own view state, and render it — formatting hex pubkeys, Unix timestamps, and verbatim `kind:0` in *your* code, because the engine hands you raw tokens and nothing else. You will also know the one performance cliff in the current bridge and how to stay off it.

## What comes back: raw rows

Every delivered event is a `Row` — verbatim, no formatting, no display concept anywhere on it:

```swift
// Packages/NMP/Sources/NMP/Row.swift
public struct Row: Sendable, Identifiable, Hashable {
    public let id: String
    public let pubkey: String        // hex, 64 chars — NOT an npub
    public let createdAt: UInt64     // Unix seconds — NOT a formatted date
    public let kind: UInt16
    public let tags: [[String]]      // each inner array is one raw tag, verbatim
    public let content: String       // for kind:0, the raw JSON string
    public let sig: String
}
```

This is bug-ledger #12 made structural: the engine emits raw tokens because the vocabulary to express presentation is *absent* from it, not because a lint forbids formatting. There is no `displayName`, no `formattedDate`, no `npub` field to reach for — and the convenience layer never re-adds one. Turning `a1b2…` into `@alice` or `1720…` into "3:14 PM" is your job, every time. That is not an oversight; it is the boundary. (See *What NMP does NOT do*.)

## How it arrives: an AsyncSequence of snapshots

`observe(_:)` returns an `NMPQuery`, which *is* an `AsyncSequence`. You iterate it directly — no observer object, no provider, no container to register:

```swift
// The whole read loop. FeedView.swift, trimmed.
let query = try engine.observe(FeedFilters.follows(kinds: [1]))
for await batch in query {
    rows = batch.rows.sorted { $0.createdAt > $1.createdAt }   // app owns order
    coverage = batch.coverage
}
```

Each element is a `RowBatch` — **the full accumulated snapshot**, never a bare delta:

```swift
public struct RowBatch: Sendable {
    public let rows: [Row]
    public let coverage: Coverage
}
```

Under the hood the engine speaks *deltas* — `Added(event)` / `Removed(id)`. The Swift bridge (`RowBridge` in `Query.swift`) accumulates them for you: it keeps an insertion-ordered `[id]` plus an `[id: Row]` map, applies each delta, and yields the current snapshot. So a consumer never reconstructs state from deltas by hand — you always get "here is everything live right now." Note what the bridge deliberately does *not* do: it accumulates in **arrival order** and applies zero sort. Ordering is a render policy the app owns; the bridge does mechanics only.

Teardown rides ARC. The subscription lives as long as the iterator does; when the query goes out of scope or its consuming `Task` is cancelled, Rust's `Drop` withdraws the demand automatically. You never *have* to call `cancel()` — it exists only for tearing down early, before ARC would.

```swift
// Re-observe on input change: build a NEW filter, let .task(id:) swap queries.
.task(id: model.kinds) { await observe() }   // old query dropped → demand withdrawn
```

### The optional `@Observable` sugar

If you would rather bind a view straight to an object than manage a `@State` array, `NMPQuerySnapshot` wraps the same sequence — but it is sugar *on top of* the primary API, not the API:

```swift
// Packages/NMP/Sources/NMP/Observable.swift
@Observable public final class NMPQuerySnapshot {
    public private(set) var rows: [Row] = []
    public private(set) var coverage: Coverage = .unknown
    public init(_ query: NMPQuery) { /* consumes the AsyncSequence in a Task */ }
}
```

The `AsyncSequence` handle is deliberately primary; the view-binding object is a thin optional layer. (Binding a view-only object as the *primary* API is the SwiftData trap the SDK avoids — see the cross-platform contract in the design guidelines.)

### Rust delivers the same shapes as deltas

On Rust you get the deltas directly — same values, platform-native delivery. There is no accumulation bridge; you fold them yourself over a blocking `recv` (D8 — blocking recv, never a poll loop):

```rust
// nmp_engine::Handle
let (_handle, rows_rx) = engine.subscribe(LiveQuery(my_follows_filter()));
// RowsMsg = (Vec<RowDelta>, QueryCoverage)
while let Ok((deltas, coverage)) = rows_rx.recv() {
    for delta in deltas {
        match delta {
            RowDelta::Added(event) => { /* insert */ }
            RowDelta::Removed(id)  => { /* drop by id */ }
        }
    }
    // render with `coverage`
}
```

Kotlin (`Flow`) and TS (`AsyncIterator`) will mirror the Swift snapshot shape when those SDKs land; today only Swift and Rust are built.

## Presentation is the app's job — worked

Because rows are raw, every screen formats in app code. From the real feed view:

```swift
private func shortHex(_ hex: String) -> String {
    guard hex.count > 16 else { return hex }
    return "\(hex.prefix(8))…\(hex.suffix(8))"
}
private func formatted(_ unixSeconds: UInt64) -> String {
    Date(timeIntervalSince1970: TimeInterval(unixSeconds))
        .formatted(date: .abbreviated, time: .shortened)
}
```

A `kind:0` profile arrives as `row.content` holding raw JSON — *you* decode it and pick which field is the display name. A `p`-tag is `["p", "<hex>", "<relay-hint>"]` — you read index 1. The engine deliberately can't do any of this for you; if it could, it would be encoding one app's display decisions as framework, which is the exact line v1's feed framework died on.

## The error + loading-state matrix

There are no exceptions sprinkled through the stream. Problems are typed states, and they arrive on two different surfaces:

| You want to know… | Where it lives | What you see |
|---|---|---|
| Did the query *fail to construct*? | `observe(_:)` throws | `NMPError` (e.g. `.invalidTagName`, `.invalidPublicKey`) — a typed, `Equatable` enum, never a crash |
| Is it *loading* / are there results yet? | the stream + `rows.isEmpty` | zero rows so far — but that is **not** the same as "no results exist" |
| Is empty *authoritative* or just *not yet known*? | `batch.coverage` | `.unknown` vs `.completeUpTo(watermark)` — the whole next chapter |
| Did a *running* query hit a problem? | `batch.coverage` + the diagnostics stream | coverage stays `.unknown`; the diagnostics screen shows why (per-relay subs, exact filters) |

The critical cell is the third row: **an empty `rows` array is a loading/coverage question, not an error.** Rendering "No results" the instant `rows.isEmpty` is true — before checking `coverage` — is a silent-stale bug. The falsifier's feed shows the honest version: an empty list renders a "waiting on relays" placeholder, and the coverage line renders `unknown` vs `complete up to <date>` explicitly. That distinction is important enough to have its own chapter — see *Coverage: empty vs unknown*.

## The performance note you must not skip

Read `RowBatch`'s contract literally: **each delta yields a full accumulated snapshot.** A long-running feed that keeps matching new events re-delivers its *entire growing row set on every single ingest* — O(rows) work per event, O(rows²) over a session. This is measured, not theoretical: against real relays, ~2,587 distinct notes produced ~3.35M raw row deliveries in 20 seconds (`docs/known-gaps.md`, a tracked P0). The snapshot bridge is ergonomic but it is *not* free at scale.

Two consequences for how you build:

1. **Diff, don't re-render wholesale.** Give SwiftUI a stable identity to diff against — `Row` is `Identifiable` by `id`, so a `List(rows)` / `ForEach(rows)` already reconciles rather than rebuilding. Do not map the whole array through an expensive formatter on every batch; format lazily per visible cell, or memoize by `row.id`.
2. **Virtualize large feeds, and bound the query.** SwiftUI's `List` and lazy stacks instantiate only visible rows — use them. Cap the query (`limit:`) so the accumulated set can't grow without bound, and lean on the *Collection observation mode* (PLANNED — *Feeds & the Collection observation mode*) once it lands: it is the engine-side answer to exactly this cliff, replacing the O(rows²) snapshot with an ordered, windowed view. Until then, treat an unbounded `observe` on a hot feed as something to profile, not something to trust.

With rows in hand and formatting owned, the one remaining question a correct reader must answer is whether "nothing here" means "nothing exists" or "we can't know yet." That is the trust chapter, next.

---

<!-- nav-footer -->
<sub>← [Live queries & the binding grammar](09-binding-grammar.md) · [Index](README.md) · [Coverage: empty vs unknown](11-coverage.md) →</sub>
