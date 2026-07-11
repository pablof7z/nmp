# Coverage: empty vs unknown

**Status: BUILT** — coverage is a real type on every delivered batch, proven by the capstone `watermark_cold_start_offline` live test. This is the trust chapter: read it before you render an empty state anywhere.

After this chapter you can tell the difference between *"there are no results"* and *"we can't know yet whether there are results"* — and render each one honestly. A builder who collapses the two ships a silent-stale bug: a screen that says "no messages," "no followers," "empty wallet," when the truth is only that the engine hasn't proven anything yet. Coverage is the API's structural defense against that bug, and it costs you one `switch`.

## Coverage is a type, not a footnote

Every `RowBatch` carries rows **and** a coverage value. They are inseparable:

```swift
// Packages/NMP/Sources/NMP/Row.swift
public enum Coverage: Sendable, Hashable {
    case completeUpTo(UInt64)   // proven: these rows are EVERYTHING up to this Unix second
    case unknown                // NOT proven: absence here means nothing
}

public struct RowBatch: Sendable {
    public let rows: [Row]
    public let coverage: Coverage
}
```

The two cases are not "verbose" and "terse" versions of the same answer. They are different *claims*:

- **`.completeUpTo(watermark)`** is an authoritative statement: *the engine can prove the visible rows are the complete set up to `watermark`.* If that set is empty, the emptiness is real — there genuinely are no matching events up to that time. This is a fact you can render and act on.
- **`.unknown`** is the absence of that proof: *the engine has not established completeness.* Zero rows under `.unknown` means *nothing*. It is not "empty" — it is "not yet." You have no license to tell the user anything is absent.

This is bug-ledger #7 made structural. `.completeUpTo` is *only* constructible from a proven watermark — the store records, per `(filter, relay)` window, how far a real EOSE or negentropy reconciliation has proven the window complete, and coverage aggregates those watermarks. "Not found" cannot be fabricated; it can only be earned. (Aggregation is unanimous, not optimistic: a query is `CompleteUpTo` only when *every* atom is proven at *every* relay in its current covering set. One lagging relay, one atom with no covering relay at all, and the whole query stays `.unknown` — a slow relay is never misread as authoritative-empty.)

## The bug this prevents

Here is the wrong reader, and it is the natural one to write:

```swift
// WRONG — silent-stale bug. Renders "No results" on a cache miss.
if rows.isEmpty {
    Text("No results")          // ← lies whenever coverage is .unknown
}
```

The instant a query opens — before any relay has answered — `rows` is empty and `coverage` is `.unknown`. This code paints "No results" over a query that is simply still working. On a cold start, on a flaky network, on a relay that's merely slow, the user sees a confident, wrong emptiness. Worse: a write built on top of a mis-read empty (a follow list that looks empty because it hasn't loaded) is how "the client wiped my follows" happens (see *Editing replaceable state safely*). Empty-vs-unknown is not a UI nicety; it is a correctness boundary.

Here is the honest reader — always branch on coverage first:

```swift
switch coverage {
case .unknown:
    ProgressView("Loading…")                    // not proven — say so, don't claim empty
case .completeUpTo(let watermark):
    if rows.isEmpty {
        ContentUnavailableView("Nothing here",   // authoritative empty — safe to render
            systemImage: "tray",
            description: Text("Complete as of \(formatted(watermark)).")
        )
    } else {
        FeedList(rows)                           // real rows, known-complete up to watermark
    }
}
```

The falsifier's feed does exactly this: it renders the coverage line verbatim — `"unknown"` vs `"complete up to \(formatted(ts))"` — so the distinction is visible on screen, permanently, as an acceptance test you can watch.

```swift
private var coverageText: String {
    switch coverage {
    case .unknown: return "unknown"
    case .completeUpTo(let ts): return "complete up to \(formatted(ts))"
    }
}
```

## Worked: the cold-start, offline, authoritative read

The reason coverage is worth a whole chapter is that it makes *offline* trustworthy. Walk the capstone test — `watermark_cold_start_offline`, the flagship falsifier for ledger #7 — because it is the exact scenario your app hits every launch.

**Phase 1 — online.** The app subscribes to account A's `kind:1` notes. Three of A's posts sit on a real relay. The engine fetches them over a plain REQ/EOSE round trip, and the batch resolves to those three rows with `CompleteUpTo(_)` — the EOSE *proved* the window complete, and that watermark is persisted to the on-disk store (redb), keyed by `(filter, relay)`.

```rust
// Phase 1 assertion (integration_capstone.rs)
ids == post_ids && matches!(coverage, QueryCoverage::CompleteUpTo(_))
// "phase 1 (online) must fetch all 3 seeded posts and reach CompleteUpTo via a real EOSE"
```

**Now go offline.** The relay is killed. The engine restarts cold on the *same* store file — zero relays reachable.

**Phase 2 — cold, offline read.** The app re-subscribes to A's notes. With no network at all, the first batch is served **the instant `subscribe` returns**, computed purely from the local store plus the router plan:

```rust
// The flagship assertion — offline, zero network round trips
ids == post_ids && matches!(coverage, QueryCoverage::CompleteUpTo(_))
// "offline cold read must be AUTHORITATIVE: CompleteUpTo from the persisted
//  watermark, serving the 3 cached rows with zero network — if this reads
//  Unknown, ledger #7 is not real"
```

The persisted watermark survived the restart. So an offline launch is not a degraded "here's some stale cache, good luck" — it is an *authoritative* read: these three rows are provably everything, as of the watermark, and your UI can render them as complete without a spinner and without lying. Cold-start offline is a feature, and coverage is what makes it one.

## The authoritative-empty case — and its control

The subtle half, and the one that separates coverage from a boolean "is-cached" flag: **a proven-empty read is `CompleteUpTo`, not `.unknown`.** If the engine has synced a window and found nothing, the honest answer is "complete, and complete means empty" — you *should* render "nothing here," because there provably is nothing.

The capstone proves this with a control account B in the same store, whose shape was *never queried* in phase 1. On the offline restart, B's read must come back **`Unknown`**, not `CompleteUpTo`:

```rust
// Control: a never-synced shape has no coverage row anywhere
rows.is_empty() && matches!(coverage, QueryCoverage::Unknown)
// "a never-synced shape must read Unknown, never CompleteUpTo — a
//  proven-empty watermark must not be confused with a genuine cache-miss"
```

Two empty result sets, two different truths: A's other windows can be authoritatively empty (synced, nothing there → `CompleteUpTo`), while B is genuinely unknown (never synced → `Unknown`). A boolean cache flag cannot tell these apart. The coverage *type* must, and does — which is precisely why it is a type and not a footnote.

## Coverage also stops redundant fetches

The watermark is not read-only trivia for your UI; the engine consults it too. Before re-fetching a window it has already proven `CompleteUpTo`, the sync planner checks the same watermark and skips the round trip. So the type that keeps your UI honest is the same type that keeps your bandwidth sane — the inverse of the empty-vs-unknown bug (redundant over-fetch) is closed by the same mechanism.

## What to hold onto

1. **Never render an empty state on `rows.isEmpty` alone.** Branch on `coverage` first. `.unknown` → loading; `.completeUpTo` + empty → an honest, authoritative empty.
2. **`.unknown` means "no license to claim absence."** Not empty, not error — *not yet*.
3. **`.completeUpTo(watermark)` is a proof, including the empty case.** A synced-and-empty window is safe to render as empty; that is different from never-synced.
4. **When coverage is stuck at `.unknown` longer than you expect,** the answer is on the diagnostics screen — which atom has no covering relay, which relay hasn't EOSE'd. See *Diagnostics & debugging: "why is my feed empty?"*, whose entire first question is this one.

---

<!-- nav-footer -->
<sub>← [Consuming results](10-consuming-results.md) · [Index](README.md) · [Feeds & the Collection mode](12-collection-mode.md) →</sub>
