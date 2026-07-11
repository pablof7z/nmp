# Your first app in 20 lines

**Status: BUILT** for Swift, direct Rust, and the minimal JVM Kotlin/Flow
package. The Kotlin snippet below illustrates the promoted target spelling, not
the current pre-promotion API. TypeScript remains uncommitted.

After this chapter you'll have seen the embedding shape — construct, set query
inputs, observe, render, publish — translated into each language's native
reactive idiom. The current examples use the Falsifier's NIP-02/kind:1 fixture;
that fixture is not a core default.

---

## The promise: pay-as-you-go, no imposed lifecycle

The design rule this chapter demonstrates is *M4's kill condition*: **one construction call, and every further feature is a method on that object.** No provider wrapper, no container, no environment injection, no mandatory background task you must schedule. Two calls for a small app; twenty for a full client; zero architecture either way.

The five moves are always the same, and every platform maps them onto its *canonical cold reactive primitive*:

1. Construct the engine (once).
2. Set the current-pubkey input when the query depends on it.
3. Build a query **value** and observe it.
4. Fold delivered rows into your own state and render raw tokens yourself.
5. (Optionally) publish a write intent and watch its receipt.

## Swift — real, runs today

Swift's dialect is `AsyncSequence`. Teardown rides ARC: drop the iterator, demand drops.

```swift
import NMP

// 1. Construct once. `indexerRelays` is typed operator discovery policy;
//    raw observe/publish has no app-expanded relay list.
let engine = try NMPEngine(config: NMPConfig(
    storePath: nil,                                   // in-memory for a demo
    indexerRelays: ["wss://purplepag.es", "wss://relay.primal.net"]))

// 2. Current pubkey is an input. Read-only browsing needs no key at all.
try engine.setActiveAccount(
    "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d")

// 3. Current Falsifier fixture: caller-selected kind:1 events by the
//    pubkeys projected from this current pubkey's NIP-02 contact list.
let follows = NMPFilter(
    kinds: [1],
    authors: .derived(
        inner: NMPFilter(kinds: [3], authors: .reactive(.activePubkey)),
        project: .tag("p")),
    limit: 200)

// 4. Observe and fold. Rows carry raw tokens; you format + order.
for await batch in try engine.observe(follows) {
    for row in batch.rows.sorted(by: { $0.createdAt > $1.createdAt }) {
        print("\(row.pubkey.prefix(8))… \(row.content)")
    }
}
```

To publish, add move 5 — the write noun:

```swift
let receipt = try await engine.publish(WriteIntent(
    pubkey: myPubkey, createdAt: UInt64(Date().timeIntervalSince1970),
    kind: 1, content: "hello nostr",
    durability: .durable, routing: .authorOutbox))
for await status in receipt.status { print(status) }
```

In SwiftUI you don't even write the `for await` by hand — a view's own `.task { for await batch in query { ... } }` is the idiomatic home, and `NMPQuerySnapshot` is optional `@Observable` sugar on top if you'd rather bind a view straight to an object. The `AsyncSequence` handle is always the primary API; the view-binding is a thin layer you can ignore.

## Rust — real, runs today

Rust's dialect is a channel `Receiver` you block on (D8: blocking `recv`, never poll). Teardown rides `Drop`. This is the shape `nmp-demo` uses; construction is more explicit because you assemble the engine's pieces yourself.

```rust
use nmp_engine::runtime::EngineThread;
use nmp_grammar::{Binding, Derived, Filter, IdentityField, Selector, TagName};
use nmp_resolver::LiveQuery;
use nmp_router::LiveDirectory;
use nmp_store::MemoryStore;
use nmp_transport::PoolConfig;
use nostr::PublicKey;
use std::collections::BTreeSet;

// 1. Construct once. The directory starts knowing ONLY the two indexers.
let indexers = ["wss://purplepag.es", "wss://relay.primal.net"]
    .iter().map(|u| u.parse().unwrap()).collect();
let (engine, handle) = EngineThread::spawn(
    MemoryStore::new(), LiveDirectory::new(indexers), 10, PoolConfig::default());

// 2. Identity is an input (read-only: no signer registered for this key).
let target = PublicKey::parse(
    "npub180cvv07tjdrrgpa0j7j7tmnyl2yr6yr7l8j4s3evf6u64th6gkwsyjh6w6").unwrap();
handle.set_active_account(Some(target));

// 3. Build the same query value.
let follows = LiveQuery(Filter {
    kinds: Some(BTreeSet::from([1u16])),
    authors: Some(Binding::Derived(Box::new(Derived {
        inner: Filter {
            kinds: Some(BTreeSet::from([3u16])),
            authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
            ..Filter::default() },
        project: Selector::Tag(TagName::new('p').unwrap()) }))),
    ..Filter::default() });

// 4. Subscribe and fold the delta batches into your own row set.
let (_query, rows_rx) = handle.subscribe(follows);
while let Ok((deltas, _coverage)) = rows_rx.recv() {
    for delta in deltas { println!("{delta:?}"); }
}
```

To publish through the current Rust surface, register a local signer and hand
`publish` a `WriteIntent`. The target facade defaults to the signer for
`$currentPubkey`, permits an identity override, and persists the obligation but
not raw secret material:

```rust
handle.add_signer(nmp_signer::LocalKeySigner::new(keys));
let rx = handle.publish(nmp_engine::outbox::WriteIntent {
    payload: nmp_engine::outbox::WritePayload::Unsigned(unsigned_event),
    durability: nmp_engine::outbox::Durability::Durable,
    routing: nmp_engine::outbox::WriteRouting::AuthorOutbox });
while let Ok(status) = rx.recv() { println!("{status:?}"); }
```

The Rust `Handle` is a `Clone + Send` value with exactly the two nouns plus identity, diagnostics, and `shutdown` — no `relays:` parameter, no open-a-REQ method. It's the fastest place to see the nouns with no UI framework in the way, which is why the manual leans on it.

## Kotlin — Flow is built; promoted target shape remains provisional

> `Packages/NMPKotlin` is a built, live-proven desktop-JVM `Flow` projection.
> It is not an Android AAR or Compose app. The names in this snippet are the
> promoted target shape and do not compile until the governed migration lands.

Kotlin's canonical cold reactive primitive is a cold `Flow`; teardown rides the collection scope. The nouns keep their names and shapes (modulo casing); only the wrapper changes. The caller applies `stateIn(WhileSubscribed)` themselves — the Room idiom verbatim — rather than NMP inventing an observer type.

```kotlin
// PLANNED — intended shape, not yet shipped.
val engine = NmpEngine(NmpConfig(
    storePath = null,
    indexerRelays = listOf("wss://purplepag.es", "wss://relay.primal.net")))

engine.setCurrentPubkey("3bf0c63f…459d")   // PLANNED target spelling

val follows = NmpFilter(
    kinds = listOf(9999),                  // caller policy, not a blessed kind
    authors = Binding.Derived(
        inner = NmpFilter(kinds = listOf(3), authors = Binding.Reactive(ActivePubkey)),
        project = Selector.Tag('p')),
    limit = 200)

// A cold Flow<RowBatch>; you own stateIn/lifecycle, exactly like Room.
engine.observe(follows).collect { batch ->
    batch.rows.sortedByDescending { it.createdAt }.forEach { render(it) }
}
```

## TypeScript — the intended shape (PLANNED)

> **PLANNED-shape.** The TS/web SDK is unconfirmed for v2. This is the intended idiom only.

TypeScript's canonical primitive is the async iterator; teardown rides breaking the `for await` loop (or an explicit `cancel()`).

```typescript
// PLANNED — intended shape, not yet shipped.
const engine = await NMPEngine.create({
  storePath: null,
  indexerRelays: ["wss://purplepag.es", "wss://relay.primal.net"],
});

engine.setCurrentPubkey("3bf0c63f…459d");   // PLANNED target spelling

const follows: NMPFilter = {
  kinds: [9999],
  authors: { derived: {
    inner: { kinds: [3], authors: { reactive: "activePubkey" } },
    project: { tag: "p" } } },
  limit: 200,
};

for await (const batch of engine.observe(follows)) {
  for (const row of [...batch.rows].sort((a, b) => b.createdAt - a.createdAt)) {
    render(row);   // raw tokens; you format
  }
}
```

## What every version has in common

Squint at the four and the invariant is obvious: **the nouns are the same value on every platform; only the delivery wrapper and the teardown edge change.** `Filter`/`Binding`/`Selector` and `Row`/`Coverage` carry identical shapes and names because they're defined once at the FFI seam. Swift gets `AsyncSequence` + ARC; Rust gets a channel + `Drop`; Kotlin gets `Flow` + collection scope; TS gets an async iterator + loop exit. None of them invents an NMP-specific observer/callback type as the primary API — that would be the SwiftData retrofit trap the design rules forbid.

And in none of them did you: pick a relay, write a subscription manager, manually re-issue a REQ when your follows changed, or format a token inside the engine. That's the twenty-line promise: a tiny grammar over two nouns, and your own code on the far side of delivery.

---

<!-- nav-footer -->
<sub>← [The two nouns](05-two-nouns.md) · [Index](README.md) · [Adding NMP to an app you own](07-brownfield.md) →</sub>
