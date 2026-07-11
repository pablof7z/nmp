# Testing an app that embeds NMP

**Status: BUILT** (the fake-engine seam `NMPEngine.init(ffi:)` over `NmpEngineProtocol` is real, as are the bounded live tests. Anchored to `Packages/NMP/Sources/NMP/Engine.swift`, `Packages/NMP/Sources/NMPFFI/nmp_ffi.swift`, and `Packages/NMP/Tests/NMPTests/`.)

After this chapter you'll be able to unit-test an app that embeds NMP without a live relay: you'll inject a fake engine that emits deterministic rows, receipts, and diagnostics; assert your view logic against golden snapshots; and keep exactly one bounded live tier for the wire path. The point is that **your app's logic is testable in milliseconds** because the engine is an injectable object, not an ambient singleton.

## The seam: `NMPEngine.init(ffi:)`

`NMPEngine` wraps a value conforming to `NmpEngineProtocol`. The public initializer constructs the real FFI engine; a second, internal initializer takes any conformer:

```swift
// Packages/NMP/Sources/NMP/Engine.swift (real)
public final class NMPEngine: Sendable {
    let ffi: NmpEngineProtocol
    public init(config: NMPConfig) throws { ffi = try /* real FFI engine */ }
    init(ffi: NmpEngineProtocol) { self.ffi = ffi }   // for tests / fakes
}
```

`NmpEngineProtocol` is the entire contract you must fake — six methods:

```swift
public protocol NmpEngineProtocol: AnyObject, Sendable {
    func addAccount(secretKey: String) throws -> String
    func observe(query: FfiFilter, observer: RowObserver) throws -> NmpQueryHandle
    func observeDiagnostics(observer: DiagnosticsObserver) -> NmpDiagnosticsHandle
    func publish(intent: FfiWriteIntent, observer: ReceiptObserver) throws
    func setActiveAccount(pubkey: String?) throws
    func shutdown()
}
```

Note the shape: `observe`/`publish`/`observeDiagnostics` each take an **observer** the engine drives. That is your injection point. A fake conforms to `NmpEngineProtocol`, captures the observer, and calls `on_batch` / `on_status` / `on_snapshot` with whatever deterministic sequence your test needs. The Swift SDK's own bridges (`RowBridge`, `ReceiptBridge`, `DiagnosticsBridge`) then turn those callbacks into the same `AsyncSequence`/`Receipt` your production code consumes — so your app code under test is byte-for-byte the real thing.

`init(ffi:)` is `internal`, so your test target imports the package with `@testable import NMP` to reach it — exactly how the SDK's own tests reach it.

## A fake engine that emits deterministic rows

```swift
@testable import NMP
import NMPFFI

final class FakeEngine: NmpEngineProtocol, @unchecked Sendable {
    // Script the batches this fake will deliver to whoever observes.
    var scriptedBatches: [(deltas: [FfiRowDelta], coverage: FfiCoverage)] = []
    private(set) var lastActivePubkey: String?

    func observe(query: FfiFilter, observer: RowObserver) throws -> NmpQueryHandle {
        // Drive the observer synchronously with the scripted sequence.
        for batch in scriptedBatches {
            observer.onBatch(deltas: batch.deltas, coverage: batch.coverage)
        }
        observer.onClosed()
        return FakeQueryHandle()      // a no-op handle; nothing to tear down
    }

    func setActiveAccount(pubkey: String?) throws { lastActivePubkey = pubkey }
    func addAccount(secretKey: String) throws -> String { "deadbeef…" }
    func observeDiagnostics(observer: DiagnosticsObserver) -> NmpDiagnosticsHandle { … }
    func publish(intent: FfiWriteIntent, observer: ReceiptObserver) throws { … }
    func shutdown() {}
}
```

Then wrap it and exercise your real app code:

```swift
func testFeedFoldsRowsInArrivalOrder() async throws {
    let fake = FakeEngine()
    fake.scriptedBatches = [
        (deltas: [.added(row(id: "a", kind: 1, content: "first"))],  coverage: .unknown),
        (deltas: [.added(row(id: "b", kind: 1, content: "second"))], coverage: .completeUpTo(1_700_000_000)),
    ]
    let engine = NMPEngine(ffi: fake)          // the fake seam

    var seen: [[Row]] = []
    var finalCoverage: Coverage = .unknown
    for await batch in try engine.observe(NMPFilter(kinds: [1])) {
        seen.append(batch.rows)
        finalCoverage = batch.coverage
    }

    // Golden assertions on YOUR fold, deterministic, no network:
    XCTAssertEqual(seen.map { $0.map(\.id) }, [["a"], ["a", "b"]])  // accumulation
    XCTAssertEqual(finalCoverage, .completeUpTo(1_700_000_000))     // empty vs unknown flipped
}
```

Because the SDK's `RowBridge` accumulates deltas into full snapshots for you, your test asserts the same accumulated shape your UI sees — including the crucial empty-vs-unknown coverage transition (see *[Coverage: empty vs unknown](11-coverage.md)*).

## Injecting deterministic receipts

The write path is the same pattern against `ReceiptObserver`. Script a `WriteStatus` sequence to test how your UI presents an in-flight publish:

```swift
func publish(intent: FfiWriteIntent, observer: ReceiptObserver) throws {
    observer.onStatus(status: .accepted)
    observer.onStatus(status: .signed(eventId: "abc…"))
    observer.onStatus(status: .routed(relays: ["wss://a", "wss://b"]))
    observer.onStatus(status: .acked(relay: "wss://a"))
    observer.onStatus(status: .gaveUp(relay: "wss://b"))
}
```

Now you can assert your UI shows "sent to 1 of 2" without a relay in sight — and prove you never confuse *enqueued* with *converged* (bug-ledger #9; see *[Writing: intents, receipts, and the durability guarantee lattice](14-writing.md)*).

## Golden-diagnostics assertions

Because diagnostics is a pure projection of real numbers, it makes an excellent golden-test surface: script a `DiagnosticsObserver` snapshot and assert your diagnostics screen (or your "is my feed routed?" logic) reads it correctly.

```swift
func observeDiagnostics(observer: DiagnosticsObserver) -> NmpDiagnosticsHandle {
    observer.onSnapshot(snapshot: FfiDiagnosticsSnapshot(
        relays: [ FfiRelayDiagnostics(
            relay: "wss://relay.example",
            wireSubCount: 1,
            authorsServed: 2,
            byLane: [],
            filters: ["{\"kinds\":[1],\"authors\":[\"…\"]}"],
            eventsByKind: [FfiKindCount(kind: 1, count: 7)],
            coverage: [FfiFilterCoverage(filter: "…", coverage: .completeUpTo(1_700_000_000))]
        )],
        uncoveredAuthorCount: 0,
        droppedMergeRules: []
    ))
    return FakeDiagnosticsHandle()
}
```

The SDK's own `DiagnosticsTests` show the shape of a real construction test: they assert a freshly built engine yields an immediate, well-formed **empty** snapshot (`relays.count == 0`), and that an unroutable literal author with no indexer surfaces as `uncoveredAuthorCount == 1` and *zero fabricated relays*. These are golden assertions over real numbers — copy the pattern for your own "empty vs unknown vs unroutable" UI states.

## No-live-relay unit tests: the discipline

Two rules keep your unit tier fast and non-flaky, both borrowed from the SDK's own tests:

1. **Never poll, always bound.** Race any stream consumption against a hard timeout with a `withTaskGroup` so a test can never hang, even if a fake misbehaves. The SDK's `firstSnapshot`/`firstNonEmptyBatch` helpers are the template — one task consumes the stream, one sleeps and returns `nil`, the group takes whichever finishes first and cancels the rest.
2. **A fake drives observers synchronously.** Your fake calls `onBatch`/`onStatus`/`onSnapshot` inline, so there's no scheduler nondeterminism — the sequence your test sees is exactly the sequence you scripted.

With those two rules, the whole app-logic tier runs with no network, in milliseconds, deterministically.

## The bounded live tier

You still want *one* tier that proves the real wire path — Swift → FFI → engine → relay — but it must be bounded so it can never hang CI. The SDK's `LiveRelayTests` is the exact model:

- Construct the real engine from **only** indexer relays (never a `relays:` param — there isn't one).
- `setActiveAccount` to a well-known read-only pubkey; observe a real derived follow-feed.
- Bound every network wait (~15–30s) and **`XCTSkip`** (don't fail) if the network didn't cooperate: "Package build + construction tests still pass independently of this network condition." A live tier that *fails* on a flaky relay would train you to ignore it; a live tier that *skips* keeps signal honest.
- Cross-check planes: the live diagnostics test asserts a relay's `eventsByKind` reports a real `kind:1` count matching the rows the query already delivered — the acceptance test made visible, run as a test (see *[Diagnostics & debugging](22-diagnostics.md)*).

## The shape of a healthy test suite

| Tier | Uses | Speed | Fails on network? |
|---|---|---|---|
| App-logic unit | `NMPEngine(ffi: FakeEngine)` — scripted rows/receipts/diagnostics | ms | never (no network) |
| Golden diagnostics | scripted `FfiDiagnosticsSnapshot` | ms | never |
| Construction/shape | real engine, no subscriptions | ms | never |
| Bounded live | real engine + real relays, timeout-guarded | seconds | **skips**, never hangs/fails |

Most of your tests live in the top rows. The seam that makes it possible is one internal initializer — `NMPEngine(ffi:)` — and one protocol you fake. Because your production app consumes the *same* SDK bridges over your fake's callbacks, a green unit suite is real evidence about real code, not a mock of it.

---

<!-- nav-footer -->
<sub>← [Cost & performance](24-performance.md) · [Index](README.md) · [Troubleshooting & FAQ](26-troubleshooting.md) →</sub>
