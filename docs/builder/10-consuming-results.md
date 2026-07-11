# Consuming results: rows, snapshots, and presentation ownership

**Status: CURRENT + TARGET.** `observe -> AsyncSequence`, `Row`, and the
latest-snapshot Swift bridge are built. The target snapshot replaces aggregate
`Coverage` with cache and per-source acquisition evidence.

After this chapter you can take a live query, iterate it, fold each delivery
into your own view state, and render raw protocol values. You will also know
which newest-state coalescing is built and which end-to-end bounds remain open.

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
    evidence = batch.evidence       // TARGET; current SDK exposes batch.coverage
}
```

Each element is a `RowBatch` — **the full accumulated snapshot**, never a bare delta:

```swift
public struct RowBatch: Sendable {  // CURRENT
    public let rows: [Row]
    public let coverage: Coverage
}
```

Under the hood the engine speaks `Added`/`Removed` deltas. The Swift bridge
applies every delta before coalescing delivery, so a slow consumer may skip
intermediate snapshots and still receive the newest complete local state. It
does not sort; ordering remains app policy.

Teardown rides ARC. The subscription lives as long as the iterator does; when the query goes out of scope or its consuming `Task` is cancelled, Rust's `Drop` withdraws the demand automatically. You never *have* to call `cancel()` — it exists only for tearing down early, before ARC would.

```swift
// Re-observe on input change: build a NEW filter, let .task(id:) swap queries.
.task(id: model.kinds) { await observe() }   // old query dropped → demand withdrawn
```

### The optional `@Observable` sugar

An optional future `@Observable` adapter may wrap the same sequence, but it is
sugar on top of the primary API, not a separate query lifecycle:

```swift
// TARGET shape, not currently shipped
@Observable public final class NMPQuerySnapshot {
    public private(set) var rows: [Row] = []
    public private(set) var evidence: QueryEvidence   // TARGET
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

The desktop-JVM Kotlin package now projects the current snapshot through cold
`Flow`; full Android and the promoted evidence shape remain open. TypeScript is
uncommitted.

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

## The result-evidence matrix

There are no exceptions sprinkled through the stream. Problems are typed states, and they arrive on two different surfaces:

| You want to know… | Where it lives | What you see |
|---|---|---|
| Did the query *fail to construct*? | `observe(_:)` throws | `NMPError` (e.g. `.invalidTagName`, `.invalidPublicKey`) — a typed, `Equatable` enum, never a crash |
| Is it *loading* / are there results yet? | the stream + `rows.isEmpty` | zero rows so far — but that is **not** the same as "no results exist" |
| What does the cache currently contain? | `snapshot.rows` + cache evidence | matching canonical rows and their persisted context |
| What are current planned sources doing? | acquisition evidence | connecting, requesting, AUTH-blocked, EOSE, unavailable, failed, or limited |
| What exactly happened per relay? | diagnostics | exact filters, lanes, connections, AUTH/errors, events, watermarks |

An empty row array means the local canonical store currently has no match. It
does not prove that no match exists globally. Apps interpret the planned-source
evidence and can say, for example, "nothing found on the sources checked" or
"one relay still needs AUTH" without NMP inventing a global health verdict.

## The performance note you must not skip

The local snapshot still costs O(rows) to materialize. Public delivery is
latest-state: intermediate snapshots may be coalesced after every underlying
delta has been applied. The engine must bound observer queues and surface local
limits instead of pushing backlog management onto each app.

Two consequences for how you build:

1. **Diff, don't re-render wholesale.** Give SwiftUI a stable identity to diff against — `Row` is `Identifiable` by `id`, so a `List(rows)` / `ForEach(rows)` already reconciles rather than rebuilding. Do not map the whole array through an expensive formatter on every batch; format lazily per visible cell, or memoize by `row.id`.
2. **Virtualize large results and request a bound when that matches the
   product.** SwiftUI's `List` and lazy stacks instantiate only visible rows.
   A caller-requested `limit` is product selection; it is not a workaround that
   permits the engine to hide its own queue/graph shortfall. Collection-mode
   windows remain planned.

With rows in hand and formatting owned, the one remaining question a correct reader must answer is whether "nothing here" means "nothing exists" or "we can't know yet." That is the trust chapter, next.

---

<!-- nav-footer -->
<sub>← [Live queries & the binding grammar](09-binding-grammar.md) · [Index](README.md) · [Coverage: empty vs unknown](11-coverage.md) →</sub>
