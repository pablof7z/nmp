# Threading, the main-thread contract & app lifecycle

**Status: BUILT** (anchored to `nmp-ffi/src/observer.rs`, `nmp-engine/src/runtime/mod.rs`, and the Swift bridges in `Packages/NMP/Sources/NMP/`. The iOS/Swift and Rust threading models are real and running; Android/TS idioms are shown as intended-shape.)

After this chapter you'll know exactly which thread your rows and receipts arrive on, why you must hop to the main actor before touching UI state, what ordering and reentrancy guarantees you get, and what happens to a live query when iOS suspends your app and later brings it back. The engine imposes **no** scene-phase hooks and no lifecycle you must adopt — but you need to know how it behaves when the OS suspends it.

## Where delivery lands: three threads you don't own

The engine's threading is entirely interior, but understanding its shape tells you where your code runs. The runtime spawns dedicated OS threads:

- The **engine thread** owns `EngineCore` and runs a blocking `recv()` loop over a command inbox (D8: blocking recv, never a poll). Nothing you write ever runs here, and nothing you do may block it.
- A **pool-bridge thread** translates transport events into engine commands.
- A **dedicated drain thread per observer** blocking-`recv`s the engine's output channel and invokes your observer callback — `on_batch` for rows, `on_snapshot` for diagnostics, `on_status` for receipts. This is deliberately *not* the engine thread: a slow consumer on your side can never stall `EngineCore`'s own loop.

So the load-bearing fact is: **delivery lands on an engine-owned callback (drain) thread, not on your UI thread.** The FFI observer doc states it directly — `on_batch` is "called once per delivered batch, in order, on a dedicated drain thread — never on the engine thread itself."

## The main-thread hop, per platform

On the drain thread, the Swift SDK's bridge does the minimum: it accumulates the delta into a full snapshot under a lock and `yield`s it into an `AsyncStream`. From there, *you* consume it — and the AsyncSequence contract is where the main-actor hop happens.

### Swift (`@MainActor`)

The `for await` loop runs wherever its enclosing task runs. SwiftUI's `.task` runs on the main actor, so assigning to `@State` inside the loop is already correct:

```swift
struct FeedView: View {
    let engine: NMPEngine
    @State private var rows: [Row] = []
    @State private var coverage: Coverage = .unknown

    var body: some View {
        List(rows) { row in Text(row.content) }
            .task {                       // main-actor context
                let query = try? engine.observe(myFilter)
                guard let query else { return }
                for await batch in query {
                    rows = batch.rows      // safe: on the main actor
                    coverage = batch.coverage
                }
            }
    }
}
```

If you consume the stream from a detached task or a background executor, you must hop yourself before touching UI-bound state:

```swift
Task.detached {
    for await batch in engine.observe(myFilter) {
        await MainActor.run { self.rows = batch.rows }
    }
}
```

The rule: **the drain thread hands you a value; crossing to the main actor is the AsyncSequence consumer's job, and SwiftUI's `.task` does it for you.**

### Kotlin (`Dispatchers.Main`) — intended shape

The Android SDK will deliver a cold `Flow`. You apply the main dispatcher and the `stateIn(WhileSubscribed)` idiom yourself — the Room pattern verbatim:

```kotlin
// PLANNED-shape (Android SDK not built yet)
engine.observe(myFilter)
    .flowOn(Dispatchers.Default)          // upstream drain work off-main
    .map { it.rows }
    .flowWithLifecycle(lifecycle)         // survives config change, pauses in bg
    .collect { rows -> adapter.submitList(rows) }   // on Dispatchers.Main
```

### TypeScript — intended shape

JS is single-threaded; the async iterator resolves microtasks on the one event loop, so there is no hop — but you still must not block, because blocking the loop blocks delivery:

```ts
// PLANNED-shape
for await (const batch of engine.observe(myFilter)) {
  render(batch.rows);   // same thread; keep it cheap
}
```

## Ordering and reentrancy

Two guarantees you can build on:

- **In-order delivery.** `on_batch` fires "once per delivered batch, in order." The Swift `AsyncStream` preserves that order to your `for await`. You will never see batch N+1 before batch N.
- **Accumulated snapshots, not raw deltas.** The Swift `RowBridge` accumulates `Added`/`Removed` deltas internally under an `NSLock` and yields the full current row set each time. So each element you receive is a complete, self-consistent snapshot — you never reconstruct state from partial deltas, and there is no torn read. (The cost of that convenience is the subject of *[Cost & performance](24-performance.md)*.)

On reentrancy: your consumer loop is linear per stream. If inside the loop you open *another* query or call `publish`, that's fine — those are just command sends onto the engine inbox; they don't reenter your observer. What you must not do is perform slow synchronous work inside the loop, because while the drain thread is blocked handing you a value, it isn't draining the next one. Do the heavy lifting after you've handed state to the UI.

## Diagnostics and receipts: same model, one wrinkle

The diagnostics and receipt streams use the identical drain-thread → AsyncStream bridge. The one difference is backpressure policy:

- **Rows and receipts** use a plain channel — every delta and every `WriteStatus` matters and none may be dropped.
- **Diagnostics** uses a *latest-wins* single-slot mailbox: a slow consumer sees the most recent snapshot next, never a backlog. That's safe because diagnostics is a recomputed projection, not a running total (see *[Diagnostics & debugging](22-diagnostics.md)*).

## App lifecycle: no hooks, but real behavior

**The engine imposes no scene-phase hooks.** There is no `onForeground`, no background-task registration you must schedule, no lifecycle you adopt. Construction is one call; there is no provider or environment wrapper. This is a deliberate kill condition — if using NMP forced a lifecycle onto your app, the library would have become a framework.

But "no hooks" doesn't mean "no behavior when the OS suspends you." Here's what actually happens on iOS:

**Backgrounding.** When iOS suspends your process, your `for await` tasks are suspended with it and the OS tears down sockets. Crucially, **demand survives.** Your live queries are still registered; the engine has not forgotten what you asked for. You did not "close" anything by being backgrounded — demand teardown rides *reference ownership* (deinit/ARC), not scene phase. As long as your views (and thus your query handles) are still alive, the demand is intact.

**Foregrounding.** When the app resumes, the engine reconnects its relays and replays: it re-establishes the subscriptions for still-live demand and syncs the gap (negentropy-first against probed relays; see *[Offline & sync](19-offline-sync.md)*). Coverage that had been proven may briefly drop to `Unknown` while it re-probes, then settle back to `CompleteUpTo`. Your `for await` loop simply starts receiving fresh batches again — you write no reconnection code.

**What you *should* do.** Because teardown is ownership-tied, the lever you control is *scope*. If you want a query to stop while backgrounded, tie its handle's lifetime to a foreground-scoped object (e.g. a screen that goes away). If you want it to keep its demand registered across a background trip so foregrounding is instant, keep the handle alive. Either way you express it as ownership, not as a lifecycle callback:

```swift
// Explicit early teardown when you DO want to drop demand now,
// rather than waiting for ARC:
query.cancel()   // idempotent; safe to never call
```

**Explicit shutdown.** `engine.shutdown()` stops the engine thread; it's idempotent and also runs on the engine object's `deinit` as a safety net, so a forgotten call never leaks the thread. You generally never call it in a normal app — the engine lives as long as your app model does.

## The takeaway

Delivery is on an engine drain thread; hopping to the main actor is the consumer's job, and SwiftUI's `.task` does it for you. Batches arrive in order and each is a full snapshot. There is no lifecycle to adopt — demand survives backgrounding, reconnection replays on foreground automatically, and the only lever you own is how long you keep your handles alive. Manage scope, not scene phase.

---

<!-- nav-footer -->
<sub>← [Diagnostics & debugging](22-diagnostics.md) · [Index](README.md) · [Cost & performance](24-performance.md) →</sub>
