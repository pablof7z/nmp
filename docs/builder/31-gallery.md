# Example gallery + graduating from the falsifier app

**Status: BUILT** — every app in this gallery is a slice of running code: the [Falsifier](../../apps/Falsifier) SwiftUI app and [`nmp-demo`](../../crates/nmp-demo) (Rust CLI), both of which run against real public relays today. The "small apps" below are framed as *one-concept extracts* of those two, each tied to a Falsifier §5 probe.

After this chapter you'll have a map from each Falsifier screen to the narrow
contract it proves. The Falsifier's social-feed-shaped probes are test fixtures,
not a preferred NMP content model.

## Why a gallery of one-concept apps

The Falsifier is deliberately built so each screen proves *one* thing from the [VISION §5](../VISION.md) table. That makes it a natural gallery: instead of one monolithic example, you get a set of focused probes you can read in isolation. Each entry below names the concept, the Falsifier probe it maps to, the single NMP call that carries it, and the file to read.

## The gallery

### 1. The current 20-line reader
**Proves:** the whole two-noun loop (observe → fold → render) with zero NMP-shaped scaffolding.
**Probe:** one depth-1 `Derived` graph using NIP-02 contact-list `p` tags.
**The one call:** `engine.observe(FeedFilters.follows(kinds:))` → `for await batch in query`.
**Read:** `apps/Falsifier/Sources/Falsifier/Views/FeedView.swift` and
`FeedFilters.swift` (the app's own reusable query declaration). The caller
chooses `model.kinds`; kind:1 is merely one value the current falsifier can
exercise. The Rust twin is `nmp-demo/src/main.rs`'s
`build_follow_feed_query()`.
```swift
let query = try engine.observe(FeedFilters.follows(kinds: model.kinds))
for await batch in query {
    rows = batch.rows.sorted { $0.createdAt > $1.createdAt }
    coverage = batch.coverage
}
```

### 2. Current-pubkey switch — "identity is an input"
**Proves today:** `setActiveAccount` re-roots the current
`Reactive(ActivePubkey)` graph.
**Target extension:** only dependent graphs reroot; a simultaneous literal
multi-account query remains live, and an accepted write's pinned signer does
not change ([ledger #10](28-patterns.md)).
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
**Proves:** REQ coalescing, source planning, caps, exact filters, event counts,
and current per-relay watermark facts are observable from engine state.
**Probe:** §5 permanent diagnostic screen.
**The one call:** `engine.observeDiagnostics()`.
**Read:** `DiagnosticsView.swift` (per-relay wire-sub count, exact wire filters, events-by-kind, per-filter coverage) and `nmp-demo`'s final-snapshot printer. Ship this screen *permanently* in your own app — it's how you (and your users) verify invisible-by-design routing.

### 6. Current coverage rendering, with a target correction
**Proves today:** rows and the current aggregate `Coverage` value travel in one
snapshot.
**Target correction:** source-scoped acquisition/shortfall evidence replaces a
global empty/complete interpretation ([ledger #7](28-patterns.md)).
**Read:** `FeedView.swift`'s current `coverageText`, then
[Query evidence](11-coverage.md) for the contract it must migrate toward.

### 7. Publish with a streaming receipt — "enqueue is not converged"
**Proves today:** per-relay outcomes live in the receipt stream, not the call's
return. Crash-safe `Accepted`, canonical pending rows, and reattachable receipt
history remain target work ([ledger #9](28-patterns.md)).
**Probe:** the compose path (`nmp-demo`'s `--nsec` publish demo).
**The one call:** `engine.publish(intent)` → `for await status in receipt.status`.
**Read:** `nmp-demo/src/main.rs`'s publish block; the Swift shape is `Receipt.swift`.

### 8. Two-indexer bootstrap — "the app does not expand routes"
**Proves:** typed indexer policy bootstraps author write-relay discovery; the
app does not compute content routes ([ledger #3](28-patterns.md)). Typed
protocol host context, such as a NIP-29 group relay, is a separate target
concept and not a raw route override.
**Probe:** §5 bootstrap-from-2-indexers.
**The config:** `NMPConfig(storePath:, indexerRelays: ["wss://purplepag.es", "wss://relay.primal.net"])` — and nothing else relay-shaped anywhere.
**Read:** `AppModel.indexerRelays` + `RelaysView.swift` (which *renders the absence* of a `relays:` parameter — the app aggregates its follows' relay lists client-side purely for display, never to route).

## Graduating from the falsifier

The Falsifier is not a template you fill in — it's a proof that a normal app can embed NMP without becoming NMP-shaped. That's exactly what makes it a good starting point: **there is no scaffolding to strip out.** To fork it:

1. **Copy the seam, drop the probes.** Keep `AppModel`'s pattern — a plain `@Observable` class you own, with `let engine: NMPEngine` constructed once in `init`. Delete the screens you don't need (`KindsEditorView`, the depth-2 group query) — nothing else depends on them, because NMP holds no cross-screen state.
2. **Replace `FeedFilters` with your own queries.** The Falsifier's declarations
   are app-owned values over the public algebra. Keep app product policy there;
   use an opt-in NIP module for exact protocol schemas, reusable fragments, or
   typed semantic operations when those modules land.
3. **Keep the diagnostics screen.** It's the cheapest high-trust thing you can ship: when a user says "my feed is empty," you read the answer off the screen instead of guessing (see [Diagnostics & debugging](22-diagnostics.md)).
4. **Bring your own architecture.** MVVM, TCA, plain SwiftUI — the engine doesn't care. The Falsifier uses plain `@Observable` because that's the least-scaffolding choice, not because NMP requires it.

What you are explicitly *not* doing when you fork: adopting a base class,
wiring an NMP provider/environment container, registering runtime route/query
callbacks, or scheduling an NMP lifecycle task. Enabling an opt-in protocol
package at build time does not change that boundary.

## The honest caveat

The Falsifier already found and closed the Swift render-storm symptom with
`FrameCoalescer` and newest-value buffering. The broader boundedness contract is
not closed: Rust row/receipt channels, transport ingestion, Android parity with
the promoted contract, and explicit shortfall still need end-to-end proof. See
[Bounded delivery](../design/bounded-delivery.md).

## What to read next

- *[Your first app in 20 lines](06-first-app.md)* — the reader from entry 1, built up from scratch per platform.
- *[Diagnostics & debugging](22-diagnostics.md)* — entry 5, in depth.
- *[Reusable declarations and protocol operations](27-recipes-and-choosing.md)* — deciding what stays app policy and what belongs to an exact NIP module.

---

<!-- nav-footer -->
<sub>← [Platform SDK guides](30-platform-guides.md) · [Index](README.md) · [Extending NMP](32-extending.md) →</sub>
