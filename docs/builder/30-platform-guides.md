# Platform SDK guides: iOS, Rust, Android, TS, TUI

**Status: BUILT for Swift and Rust** (real, running SDKs — examples below are the current API). **PLANNED-shape for Android, TypeScript, and TUI** — those sections show the *intended* idiom, clearly marked; the SDKs aren't built yet.

After this chapter you'll know the idiomatic delivery, ownership/teardown idiom, and sugar layers for each platform — and you'll be able to read just the one section for the platform you ship on. Read exactly one; the nouns are identical across all five, only the reactive wrapper and the ownership edge change.

## The invariant across all platforms

Before the per-platform sections, hold this: **the nouns are the invariant; the delivery is the dialect.** `NMPFilter`/`Binding`/`Selector`, `WriteIntent`/durability/routing, `Row`/`Coverage`, receipt states, and diagnostics rows are the *same serializable values* on every platform, defined once at the FFI seam. What varies is only (a) the platform's canonical reactive primitive and (b) how teardown rides the platform's natural ownership edge. Three rules hold everywhere:

1. **Detachable handle first, view-binding sugar second.** The reactive handle (`AsyncSequence`/`Flow`/async iterator/`Stream`) is the primary API. `@Observable`/`StateFlow`/signal adapters are thin optional layers on top — never the primary surface (the SwiftData retrofit lesson: a view-only binding as the primary API is a trap that takes years to undo).
2. **Teardown rides ownership, and explicit `cancel()` is never required.** Dropping the handle drops the demand. The engine's teardown-with-grace debounce makes every platform's natural drop safe.
3. **No imposed lifecycle.** One construction call; every feature is a method on that object. No provider/container/scene-phase wrapper on any platform.

---

## iOS / Swift — BUILT

**Delivery: `AsyncSequence`.** `engine.observe(filter)` returns an `NMPQuery`, which *is* an `AsyncSequence<RowBatch>`. Iterate it directly; each element is the full accumulated snapshot (the bridge folds `Added`/`Removed` deltas for you).

```swift
import NMP

let engine = try NMPEngine(config: NMPConfig(
    storePath: storeURL.path,
    indexerRelays: ["wss://purplepag.es", "wss://relay.primal.net"]
))

// Read: iterate the AsyncSequence. Presentation is yours.
let query = try engine.observe(FeedFilters.follows(kinds: [1]))
for await batch in query {
    self.rows = batch.rows.sorted { $0.createdAt > $1.createdAt }
    self.coverage = batch.coverage        // branch on .unknown vs .completeUpTo
}
```

**Teardown: deinit/ARC.** Demand drops when the last strong reference to the underlying handle is released — the query goes out of scope, or its iteration `Task` is cancelled and drops the iterator. In SwiftUI, bind the loop to a view's lifetime with `.task` / `.task(id:)` and you never call `cancel()`:

```swift
.task(id: model.kinds) {          // re-observes when kinds change; old query torn down
    await observe()               // editing a filter = a NEW value, never mutating a running query
}
```

**Identity & writes.**
```swift
let pubkey = try await engine.addAccount(secretKey: nsecOrHex)   // key crosses once, lives engine-side
try engine.setActiveAccount(pubkey)                             // re-roots every reactive query

let receipt = try await engine.publish(WriteIntent(
    pubkey: pubkey, createdAt: now, kind: 1, content: "gm",
    durability: .durable, routing: .authorOutbox))
for await status in receipt.status {                            // convergence lives in the STREAM
    print(status)                                              // .accepted → .signed → .acked(relay:) …
}
```

**Sugar layer.** Fold `batch.rows` into `@State`/`@Observable` (as the Falsifier does). An `@Observable` snapshot adapter is a thin layer *on top of* the `AsyncSequence`, never a replacement for it.

**Gotcha.** An unbounded query (no `limit`) replaying deep history can peg the main thread on first observe, because each delta currently re-delivers the full snapshot and drives a SwiftUI re-render. Add a `limit` to bound your own query; the deeper fix (batching/coalescing on the wire and internal discovery scoping) is tracked in [`docs/known-gaps.md`](../known-gaps.md).

---

## Rust — BUILT

**Delivery: a `Handle` + channels.** `EngineThread::spawn(...)` returns a `Handle`; `handle.subscribe(query)` returns a query handle plus a `Receiver` of `(deltas, coverage)`. This is the lowest-level view of the nouns — the fastest place to see them with no UI framework in the way, and the substrate the TUI renders.

```rust
use nmp_engine::runtime::EngineThread;
use nmp_engine::core::RowDelta;
use nmp_engine::outbox::{Durability, WriteIntent, WritePayload, WriteRouting};

let (engine_thread, handle) = EngineThread::spawn(store, directory, ROUTER_CAP, PoolConfig::default());
handle.add_signer(signer);
handle.set_active_account(Some(target));

let (_query_handle, rows_rx) = handle.subscribe(my_follows);   // my_follows: LiveQuery
while let Ok((deltas, coverage)) = rows_rx.recv_timeout(remaining) {
    for delta in deltas {
        match delta {
            RowDelta::Added(event) => { /* accumulate; format is yours */ }
            RowDelta::Removed(id)  => { /* drop id */ }
        }
    }
    // coverage: Unknown vs CompleteUpTo(_)
}
```

**Writes** return a receipt receiver you drain for `WriteStatus`:
```rust
let receipt_rx = handle.publish(WriteIntent {
    payload: WritePayload::Unsigned(unsigned),
    durability: Durability::Durable,
    routing: WriteRouting::AuthorOutbox,
});
while let Ok(status) = receipt_rx.try_recv() { println!("[receipt] {status:?}"); }
```

**Teardown: `Drop`.** Dropping `_query_handle` withdraws demand. `handle.shutdown(); engine_thread.join();` stops the engine cleanly. Note the Rust side works in the *raw* grammar types (`Filter`, `Binding::Derived`, `Selector::Tag`, `IdentityField::ActivePubkey`) rather than an ergonomic wrapper — see `nmp-demo/src/main.rs` for a complete end-to-end program.

**Diagnostics.** `handle.observe_diagnostics()` returns a *latest-value-wins* receiver — drain it on its own thread (never poll, per D8) and read the shared slot. It carries per-relay wire-sub count, exact filter JSON, events-by-kind, and coverage.

---

## Android / Kotlin — PLANNED-shape

The Kotlin SDK is the M6 cross-platform proof; it is **not built yet**. The intended idiom:

**Delivery: cold `Flow`.** `engine.observe(filter)` returns a cold `Flow<RowBatch>`. The *caller* applies `stateIn(scope, WhileSubscribed())` — the Room idiom verbatim — so the engine never invents an observer type.

```kotlin
// PLANNED-shape — not yet shipped.
val rows: StateFlow<List<Row>> =
    engine.observe(FeedFilters.follows(kinds = listOf(1)))
        .map { it.rows.sortedByDescending(Row::createdAt) }
        .stateIn(viewModelScope, SharingStarted.WhileSubscribed(5_000), emptyList())
```

**Teardown: collection scope.** Demand drops when the collecting coroutine's scope ends (`WhileSubscribed` is the refcount edge the engine's teardown-with-grace listens to). No explicit close.

**Writes:** `engine.publish(intent)` returns a `Flow<WriteStatus>` you collect for convergence — same states as Swift's `Receipt.status`.

---

## TypeScript / web — PLANNED-shape (unconfirmed for v2)

The TS SDK is **not built** and web is likely out of v2 — confirm before starting one. The two-noun surface is wasm-compatible by construction (serializable values), so deferral costs nothing structural. Intended idiom:

**Delivery: async iterator.**
```ts
// PLANNED-shape — not yet shipped, and web may be out of v2.
for await (const batch of engine.observe(followsFilter([1]))) {
  setRows([...batch.rows].sort((a, b) => b.createdAt - a.createdAt));
  setCoverage(batch.coverage);      // "unknown" | { completeUpTo: number }
}
```

**Teardown:** `break`ing the `for await` (or an `AbortController`) drops the iterator and the demand. A signal/store adapter (framework-specific) is the thin sugar layer on top.

---

## TUI / CLI — PLANNED-shape (Rust handle is BUILT; the render loop is the pattern)

`nmp-demo` is a real, running CLI consumer built on the Rust `Handle` above — that part is BUILT. A *full-screen TUI* (ratatui-style render loop) is the intended shape, not yet a shipped SDK:

**Delivery: render the Rust handle as text.** The TUI is the Rust `Handle` with a draw loop. Drain the row and diagnostics receivers into your app model; redraw on change.

```rust
// PLANNED-shape for a full TUI; the receivers themselves are BUILT (see nmp-demo).
loop {
    if let Ok((deltas, coverage)) = rows_rx.try_recv() { model.apply(deltas, coverage); }
    if let Some(diag) = latest_diag.lock().unwrap().clone() { model.diag = diag; }
    terminal.draw(|f| render(f, &model))?;              // format tokens → text HERE
    if event::poll(tick)? { if handle_input()? { break; } }
}
```

**Teardown:** drop the handles, then `handle.shutdown(); engine_thread.join();`. The TUI is the manual's recommended place to *see* the nouns without a UI framework in the way — diagnostics render as a plain table.

---

## Parity is tracked, not assumed

A capability "exists" for a platform only when its SDK is built or it's explicitly marked platform-pending here. Today: **Swift and Rust are BUILT**; Android is M6; TS is unconfirmed for v2; a full TUI is a render-loop pattern over the BUILT Rust handle. When you read a worked example elsewhere in this manual, the Swift and Rust versions are runnable; the Kotlin/TS/TUI versions show the idiom and are marked accordingly.

## What to read next

- *[Your first app in 20 lines](06-first-app.md)* — the same nouns per platform, side by side.
- *[Threading, the main-thread contract & app lifecycle](23-threading-lifecycle.md)* — the ownership-edge details each idiom relies on.
- *[Consuming results](10-consuming-results.md)* — folding batches into your own state on any platform.

---

<!-- nav-footer -->
<sub>← [What NMP does NOT do](29-not-do.md) · [Index](README.md) · [Example gallery](31-gallery.md) →</sub>
