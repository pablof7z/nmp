# Example gallery + graduating from the falsifier app

**Status: BUILT** — every app in this gallery is a slice of running code: the [Falsifier](../../apps/Falsifier) SwiftUI app and [`nmp-demo`](../../nmp-demo) (Rust CLI), both of which run against real public relays today. The "small apps" below are framed as *one-concept extracts* of those two, each tied to a Falsifier §5 probe.

After this chapter you'll have a menu of minimal apps — each proving exactly one concept — to copy from, and you'll know how to fork the Falsifier as a scaffolding-free starting point for your own app.

## Why a gallery of one-concept apps

The Falsifier is deliberately built so each screen proves *one* thing from the [VISION §5](../VISION.md) table. That makes it a natural gallery: instead of one monolithic example, you get a set of focused probes you can read in isolation. Each entry below names the concept, the Falsifier probe it maps to, the single NMP call that carries it, and the file to read.

## The gallery

### 1. The 20-line reader — "follows' notes, live"
**Proves:** the whole two-noun loop (observe → fold → render) with zero NMP-shaped scaffolding.
**Probe:** §5 "my follows" source mode, depth-1 grammar.
**The one call:** `engine.observe(FeedFilters.follows(kinds:))` → `for await batch in query`.
**Read:** `apps/Falsifier/Sources/Falsifier/Views/FeedView.swift` (28 lines of NMP; the rest is presentation) and `FeedFilters.swift` (the app's own query recipe). The Rust twin is `nmp-demo/src/main.rs`'s `build_follow_feed_query()`.
```swift
let query = try engine.observe(FeedFilters.follows(kinds: model.kinds))
for await batch in query {
    rows = batch.rows.sorted { $0.createdAt > $1.createdAt }
    coverage = batch.coverage
}
```

### 2. Multi-account switch — "identity is an input"
**Proves:** account switch is one call; the engine re-roots the whole binding graph; no cross-account leakage ([ledger #10](28-patterns.md)).
**Probe:** §5 multi-nsec login + runtime switch.
**The one call:** `engine.setActiveAccount(pubkey)`.
**Read:** `AccountsView.swift` + `AppModel.swift`. Note the account *list, labels, and "which is active"* are the app's own state — NMP tracks none of it. Read-only browsing (a pubkey with no key) is a first-class case.

### 3. Live-editable queries — "descriptors are values"
**Proves:** changing a filter recompiles demand with no "edit a running query" verb — you build a *new* value and re-observe.
**Probe:** §5 user-editable kinds at runtime.
**The pattern:** `.task(id: model.kinds) { await observe() }` — SwiftUI tears down the old query and opens a fresh one when the value changes.
**Read:** `KindsEditorView.swift` + the `.task(id:)` in `FeedView.swift`.

### 4. Depth-2 grammar — "one engine, heterogeneous shapes"
**Proves:** the binding grammar is general, not two hardcoded reads — a NIP-29 groups-I'm-in feed with an identity root two hops up.
**Probe:** §5 source mode 2 (inner `kinds:[39002], #p:[$active]` → project `Tag(d)` → outer `#d := Derived(…)`).
**The shape:** a `Derived` binding in a *tag* position, projecting `Tag(d)`.
**Read:** `FeedFilters.swift`'s group query (the same `.derived(inner:project:)` algebra as `follows`, different projection). This is the probe that retires the old repo's unproven `dependent_interests.rs` special case.

### 5. The permanent diagnostics screen — "the acceptance test on screen"
**Proves:** REQ coalescing, 2-relay-min coverage, cap, and cache-miss authority are all *observable* — every number read off real engine state, never estimated.
**Probe:** §5 permanent diagnostic screen.
**The one call:** `engine.observeDiagnostics()`.
**Read:** `DiagnosticsView.swift` (per-relay wire-sub count, exact wire filters, events-by-kind, per-filter coverage) and `nmp-demo`'s final-snapshot printer. Ship this screen *permanently* in your own app — it's how you (and your users) verify invisible-by-design routing.

### 6. Coverage-aware empty state — "empty vs unknown"
**Proves:** you branch on `Coverage`, never on an empty array alone ([ledger #7](28-patterns.md)).
**Probe:** §5 cache-miss authority (rendered).
**The pattern:** `switch coverage { case .unknown: …spinner…; case .completeUpTo(let ts): …authoritative empty… }`.
**Read:** `FeedView.swift`'s `coverageText` + `ContentUnavailableView` overlay.

### 7. Publish with a streaming receipt — "enqueue is not converged"
**Proves:** a durable write's convergence lives in the receipt *stream*, not the call's return ([ledger #9](28-patterns.md)).
**Probe:** the compose path (`nmp-demo`'s `--nsec` publish demo).
**The one call:** `engine.publish(intent)` → `for await status in receipt.status`.
**Read:** `nmp-demo/src/main.rs`'s publish block; the Swift shape is `Receipt.swift`.

### 8. Two-indexer bootstrap — "you never pick relays"
**Proves:** the engine self-discovers every author's write relays from exactly two indexer relays; zero hardcoded content relays ([ledger #3](28-patterns.md)).
**Probe:** §5 bootstrap-from-2-indexers.
**The config:** `NMPConfig(storePath:, indexerRelays: ["wss://purplepag.es", "wss://relay.primal.net"])` — and nothing else relay-shaped anywhere.
**Read:** `AppModel.indexerRelays` + `RelaysView.swift` (which *renders the absence* of a `relays:` parameter — the app aggregates its follows' relay lists client-side purely for display, never to route).

## Graduating from the falsifier

The Falsifier is not a template you fill in — it's a proof that a normal app can embed NMP without becoming NMP-shaped. That's exactly what makes it a good starting point: **there is no scaffolding to strip out.** To fork it:

1. **Copy the seam, drop the probes.** Keep `AppModel`'s pattern — a plain `@Observable` class you own, with `let engine: NMPEngine` constructed once in `init`. Delete the screens you don't need (`KindsEditorView`, the depth-2 group query) — nothing else depends on them, because NMP holds no cross-screen state.
2. **Replace `FeedFilters` with your own queries.** The Falsifier's `follows(kinds:)` and group query are the app's *own* recipes over the public algebra (NMP core exposes nothing named "follows"). Write yours the same way, or enable a NIP module when the recipe layer lands (see [The batteries: recipes](27-recipes-and-choosing.md)).
3. **Keep the diagnostics screen.** It's the cheapest high-trust thing you can ship: when a user says "my feed is empty," you read the answer off the screen instead of guessing (see [Diagnostics & debugging](22-diagnostics.md)).
4. **Bring your own architecture.** MVVM, TCA, plain SwiftUI — the engine doesn't care. The Falsifier uses plain `@Observable` because that's the least-scaffolding choice, not because NMP requires it.

What you are explicitly *not* doing when you fork: adopting a base class, wiring a provider/environment container, registering modules, or scheduling a background task. If your fork starts growing any of those to satisfy NMP, that's the [M5 kill condition](29-not-do.md) firing on your desk — stop and check, because it shouldn't.

## The honest caveat

The Falsifier is where dogfooding finds real gaps. The one you'll hit first: an *unbounded* query (no `limit`) replaying deep history can saturate the main thread for a minute or two on first observe, because each delta re-delivers the full snapshot into a SwiftUI re-render. Bound your own queries with a `limit`; the deeper fix is tracked in [`docs/known-gaps.md`](../known-gaps.md). The gallery apps are real, which means they show you the rough edges too — that's the point of a falsifier.

## What to read next

- *[Your first app in 20 lines](06-first-app.md)* — the reader from entry 1, built up from scratch per platform.
- *[Diagnostics & debugging](22-diagnostics.md)* — entry 5, in depth.
- *[The batteries: recipes, and choosing](27-recipes-and-choosing.md)* — turning `FeedFilters`-style app recipes into shared modules.

---

<!-- nav-footer -->
<sub>← [Platform SDK guides](30-platform-guides.md) · [Index](README.md) · [Extending NMP](32-extending.md) →</sub>
