# Platform projections of one facade

**Status: CURRENT + TARGET.** Swift `AsyncSequence`, direct Rust, and the
desktop-JVM Kotlin `Flow` package are built and live-proven. Full Android/AAR/
Compose remains open. Public target records remain governed and provisional;
TypeScript/TUI are not v2 commitments.

The architectural invariant is one Rust facade that preserves NMP's store,
demand, routing, write, evidence, and diagnostics rules. FFI and native SDKs
project it; they do not assemble mechanism crates differently or invent a
second lifecycle.

## Cross-platform rules

1. Live query and write intent are the two workload nouns.
2. Native reactive handles are primary: `AsyncSequence` on Swift, `Flow` on
   Kotlin, and the canonical Rust stream/receiver surface.
3. Query and diagnostics observations are bounded latest-state streams.
4. Receipt facts are durable and reattachable, not an unbounded observer log.
5. Handle ownership controls observation scope; no NMP provider or scene-phase
   framework is required.
6. Demand, draft/context, receipt, evidence, and diagnostics values have one
   semantic definition across projections.
7. Standard platform signer providers may wrap Keychain/Keystore, while the
   app owns identity policy and custom providers.

## Swift today

The current SDK exposes `NMPEngine`, `NMPFilter`, `NMPQuery: AsyncSequence`,
`Receipt`, and `NMPDiagnostics: AsyncSequence`:

```swift
let query = try engine.observe(selection)
for await snapshot in query {
    rows = snapshot.rows
    currentCoverage = snapshot.coverage // current aggregate API, not global truth
}

for await diagnostics in engine.observeDiagnostics() {
    diagnosticState = diagnostics
}
```

The bridge applies every row delta, frame-coalesces output, and buffers newest
state. SwiftUI's `.task` supplies ordinary observation scope and main-actor
delivery. No app-side debounce is required for the built replay fix.

Current `addAccount`/`setActiveAccount` uses a local engine-side signer. The
target projects `setCurrentPubkey`, standard secure signer providers, default
publish identity plus explicit override, durable `AwaitingSigner`, and
source-scoped query evidence.

## Direct Rust today and target facade

The current `Handle` exposes raw grammar values, row deltas, receipt statuses,
and latest-wins diagnostics. Consumers block on receive or bridge receivers
into their own event loop; production code does not spin on `try_recv` plus a
timer.

The target makes one canonical invariant-preserving facade the supported Rust
entry point. Mechanism crates remain internal composition units. That facade is
also what UniFFI projects, so a direct Rust app cannot bypass durability,
context, limits, or diagnostics rules that Swift/Kotlin receive.

## Kotlin JVM today and Android target

`Packages/NMPKotlin` now exposes cold `Flow` values through the same UniFFI
facade. `callbackFlow` plus deterministic `awaitClose { handle.cancel() }`
proved a clean lifecycle; `Flow.conflate()` supplies bounded newest-state
delivery for the current projection. The package is JVM-only, not an Android
AAR or Compose integration.

The promoted target extends those built mechanics with source evidence, signer
providers, durable receipts, and the canonical facade:

```kotlin
// TARGET shape; names provisional.
engine.observe(demand)
    .collect { snapshot ->
        render(snapshot.rows)
        renderSourceEvidence(snapshot.acquisition)
    }

engine.publish(draft, asIdentity = optionalOverride)
    .receiptFacts()
    .collect(::renderReceipt)
```

The future Android SDK owns bounded newest-state buffering and a standard
Keystore-backed signer provider. App coroutine scope controls observation. The platform test must prove
provider reattachment, slow-consumer boundedness, exact module composition, and
facade parity rather than merely compiling generated bindings.

## Other consumers

A CLI/TUI can consume the canonical Rust facade by blocking on engine events
and forwarding them into one application event loop. It must not implement a
fixed-rate `try_recv` polling loop.

TypeScript/web remains uncommitted. Serializability is useful, but it is not a
reason to promise another SDK before the promoted Swift/Kotlin contract earns
full parity.

## Parity gate

A public capability is cross-platform only when its behavioral tests agree on:

- descriptor identity and printed expansion;
- cached rows plus source/shortfall evidence;
- pending/signed row identity and receipt persistence;
- signer default, override, pinning, and reattachment;
- protocol-module unsigned bytes and contextual route facts;
- bounded slow-observer behavior; and
- permanent diagnostics facts.

Generated types alone are not parity.

---

<!-- nav-footer -->
<sub>← [What NMP does not own](29-not-do.md) · [Index](README.md) · [Example gallery](31-gallery.md) →</sub>
