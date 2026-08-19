# Embedding NMP in an app

> **Provisional target API.** Names below are intentionally coherent, not
> frozen.

The same five moves apply:

1. construct one engine;
2. populate the whole session and choose its current account;
3. observe a demand;
4. fold snapshots into app state; and
5. publish intents and observe receipts.

The `nmp` facade owns construction. Applications do
not assemble store, router, resolver, signer, and transport crates themselves:

```rust
use nmp::{Demand, Engine, EngineConfig, Filter, ReadRouting};

let engine = Engine::new(EngineConfig::persistent(path, bootstrap))?;
engine.set_current_pubkey(Some(selected_pubkey))?;

let demand = Demand {
    selection: Filter::literal_kinds_and_authors([app_kind], selected_authors),
    access: Default::default(),
};

let mut snapshots = engine.observe(LiveQuery::single(demand), None)?;
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
facade owns construction and every safety invariant.

## What the facade owes

Regardless of the exact spelling, the facade guarantees:

- descriptor identity and printed binding expansion;
- rows plus cache/acquisition/shortfall evidence;
- accepted pending row and signature promotion;
- current-account provider selection, per-write override, pinning, and provider
  availability resumption;
- typed protocol context and final unsigned bytes;
- per-relay receipt facts; and
- diagnostics and bounded-delivery behavior.

## Platforms not promised

Serializability does not imply a TypeScript/web SDK commitment. New platform
integrations are added only when their native lifetime, persistence,
secure-capability, and bounded-delivery behavior can preserve the same
contract.

---

<sub>← [Two nouns and ownership](05-two-nouns.md) · [Index](README.md) · [Brownfield adoption](07-brownfield.md) →</sub>
