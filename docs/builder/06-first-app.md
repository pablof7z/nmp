# One semantic API, native platform shapes

> **Provisional target API.** Rust owns the semantic facade. Swift and Kotlin
> project it into their native observation and lifetime primitives. Names below
> are intentionally coherent, not frozen.

The same five moves exist everywhere:

1. construct one engine;
2. provide reactive inputs and capabilities;
3. observe a demand;
4. fold snapshots into app state; and
5. publish intents and observe receipts.

## Swift

Swift uses `AsyncSequence` and ARC:

```swift
let engine = try NMPEngine(configuration: configuration)
try engine.setCurrentPubkey(selectedPubkey)

let demand = NMPDemand(
    selection: NMPFilter(
        kinds: .literal([appKind]),
        authors: .literal(selectedAuthors)
    ),
    source: .authorOutboxes,
    access: .public
)

for await snapshot in try engine.observe(demand) {
    model.rows = snapshot.rows
    model.acquisition = snapshot.acquisition
    model.shortfall = snapshot.shortfall
}
```

Publishing uses the same engine and a native async receipt:

```swift
let receipt = try engine.publish(.init(
    draft: draft,
    durability: .durable,
    signer: nil                 // default signer for current pubkey
))

for await fact in receipt.facts {
    model.apply(fact)
}
```

A SwiftUI `.task` or app-owned model task supplies observation scope. Optional
`@Observable` conveniences may fold the sequence, but `AsyncSequence` remains
the primitive API. NMP does not require an environment key or scene hook.

## Kotlin

Kotlin uses cold `Flow` and coroutine cancellation:

```kotlin
val engine = NmpEngine(configuration)
engine.setCurrentPubkey(selectedPubkey)

val demand = Demand(
    selection = Filter(
        kinds = Binding.literal(setOf(appKind)),
        authors = Binding.literal(selectedAuthors)
    ),
    source = SourceAuthority.AuthorOutboxes,
    access = AccessContext.Public
)

engine.observe(demand).collect { snapshot ->
    state.update {
        it.copy(
            rows = snapshot.rows,
            acquisition = snapshot.acquisition,
            shortfall = snapshot.shortfall
        )
    }
}
```

```kotlin
engine.publish(
    WriteIntent(draft = draft, durability = Durability.Durable)
).facts.collect(receiptModel::apply)
```

The app chooses `stateIn`, `shareIn`, and coroutine scope. NMP supplies bounded
newest-state delivery and deterministic cancellation; it does not supply a
ViewModel base class or Compose provider.

## Rust

Direct Rust uses the same canonical facade that FFI projects. Applications do
not assemble store, router, resolver, signer, and transport crates themselves:

```rust
use nmp::{Demand, Engine, EngineConfig, Filter, SourceAuthority};

let engine = Engine::new(EngineConfig::persistent(path, bootstrap))?;
engine.set_current_pubkey(Some(selected_pubkey))?;

let demand = Demand {
    selection: Filter::literal_kinds_and_authors([app_kind], selected_authors),
    source: SourceAuthority::AuthorOutboxes,
    access: Default::default(),
};

let mut snapshots = engine.observe(demand)?;
while let Some(snapshot) = snapshots.recv() {
    app_state.apply(snapshot);
}
```

```rust
let mut receipt = engine.publish(WriteIntent::durable(draft))?;
while let Some(fact) = receipt.recv() {
    app_state.apply_receipt(fact);
}
```

The exact Rust stream/receiver spelling may change. The boundary may not: one
facade owns construction and every safety invariant inherited by FFI.

## Semantic parity

Native syntax may differ, but these values and outcomes must agree:

- descriptor identity and printed binding expansion;
- rows plus cache/acquisition/shortfall evidence;
- accepted pending row and signature promotion;
- default signer, per-write override, pinning, and reattachment;
- typed protocol context and final unsigned bytes;
- per-relay receipt facts; and
- diagnostics and bounded-delivery behavior.

Generated bindings compiling is not parity. Behavioral falsifiers across the
three entry points are the gate.

## Platforms not promised

Serializability does not imply a TypeScript/web SDK commitment. New projections
are added only when their native lifetime, persistence, secure-capability, and
bounded-delivery behavior can preserve the same contract.

---

<sub>← [Two nouns and ownership](05-two-nouns.md) · [Index](README.md) · [Brownfield adoption](07-brownfield.md) →</sub>
