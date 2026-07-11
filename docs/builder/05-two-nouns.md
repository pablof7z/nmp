# The two nouns and the ownership table

**Status: CURRENT + TARGET.** Both primary operations run today. Their target
evidence, durable-acceptance, signer-override, and module contracts remain
partly unbuilt.

After this chapter you'll be able to name where any piece of an NMP app belongs — engine, your app, or the UI framework — without guessing, because the whole surface is two nouns and you'll know what each one owns.

---

## Everything is one of two nouns

NMP's public surface is deliberately tiny. There are exactly two things you *do*:

1. **A live query** — a closed selection plus source authority and access
   context, handed to `observe`. It yields rows plus cache/acquisition evidence.
2. **A write intent** — an immutable draft plus durability, routing context,
   and optional signer override, handed to `publish`. It yields a durable receipt.

That's it. Everything else you might reach for is *not a third noun*:

- **Current pubkey** is a reactive input and default signer selection, not a
  global authority over every query and write.
- **Capabilities** (signer, decrypt, AUTH policy) are *plug points* — objects the engine invokes at the right moment, that you configure but don't call.
- **Diagnostics** are a *projection* — a read-only view of what the other planes did.

If you ever feel you need a "session," a "feed manager," a "subscription object," or a "relay pool" as a first-class thing you own, stop: that instinct is the old client-framework fragmentation returning. The answer is always a query you observe, a write you intend, or configuration of the machinery that serves those two. When you think you need a third noun, you almost always need a differently-shaped *value* for one of the two you have.

## The read noun, concretely

Here is the read noun in Swift and in Rust. Same value, two dialects:

```swift
// Swift — kind:9999 events by whoever the current pubkey's kind:3 names.
let filter = NMPFilter(
    kinds: [9999],
    authors: .derived(
        inner: NMPFilter(kinds: [3], authors: .reactive(.activePubkey)),
        project: .tag("p")
    ),
    limit: 200
)
for await batch in try engine.observe(filter) {
    render(batch.rows)          // your code, after delivery
    show(batch.evidence)        // TARGET: cache + current-source evidence
}
```

```rust
// Rust — the identical value through the Handle.
let query = LiveQuery(Filter {
    kinds: Some(BTreeSet::from([9999u16])),
    authors: Some(Binding::Derived(Box::new(Derived {
        inner: Filter {
            kinds: Some(BTreeSet::from([3u16])),
            authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
            ..Filter::default()
        },
        project: Selector::Tag(TagName::new('p').unwrap()),
    }))),
    ..Filter::default()
});
let (_handle, rows_rx) = handle.subscribe(query);
while let Ok((deltas, coverage)) = rows_rx.recv() { /* fold + render */ }
```

The field names and shapes are identical because they're serializable values defined once at the FFI seam. What differs is only the reactive wrapper (`AsyncSequence` vs a channel `Receiver`) and the ownership idiom. That's the cross-platform contract.

## The write noun, concretely

```swift
let receipt = try await engine.publish(draft)              // current signer
let other = try await engine.publish(draft, as: podcastId) // explicit override
for await status in receipt.status {
    // .accepted → .signed → .routed → .sent(relay) → .acked(relay) ...
}
```

Apps do not expand ordinary routing into relay lists. Typed protocol context may
carry source authority, such as a NIP-29 group host; that is not a generic relay
override.

## The ownership table

This is the whole mental model on one page. For any feature, find the row.

| Concern | NMP owns (engine) | Your app owns | The UI framework owns |
|---|---|---|---|
| **Which queries exist, and when** | — | ✅ you build `NMPFilter` values and call `observe` | — |
| **Binding resolution** (`Derived`, `Reactive`, `SetOp`) | ✅ resolves in-engine, incrementally | — | — |
| **Relay routing** (outbox, lanes, fan-out cap, coalescing) | ✅ compiler output from typed facts and validated protocol context | ❌ no generic app-expanded route list | — |
| **Sync** (negentropy, coverage watermarks) | ✅ | — | — |
| **Row delivery + source evidence** | ✅ delivers raw rows + cache/acquisition facts | ✅ interprets and folds them into view state | — |
| **Ordering / sorting** | ❌ delivers a live set, no order | ✅ ordering is render policy | — |
| **Formatting** (hex→npub, dates, kind:0 fields) | ❌ raw tokens only (ledger #12) | ✅ all of it, in app code | — |
| **Signing** | ✅ defaults to current pubkey, pins accepted identity, supports override | ✅ registers identities and chooses an override when exceptional | — |
| **Write routing + durability + acks** | ✅ | ✅ you compose *what* to write and when | — |
| **Identity / re-rooting** | ✅ re-resolves only dependent demand | ✅ current-pubkey and account UX | — |
| **Diagnostics numbers** | ✅ every figure from real engine state | ✅ rendering the screen | ✅ list/section chrome |
| **View lifecycle, `@State`, teardown** | ✅ demand drops when the handle is released | ✅ where the engine object lives | ✅ `.task`, ARC, scene phase |

Two lines are worth memorizing because they catch the most people:

- **Ordering and formatting are yours.** The engine hands you a live set of raw-token rows. It is not being lazy — a blessed sort order or a blessed date format is *one app's product decision* pretending to be framework, and baking it in is exactly the bug that killed the v1 feed layer. See how the timeline chapter sorts by `createdAt` and shortens hex pubkeys in *app* code.
- **Raw app-expanded routing is absent.** Engine discovery and typed protocol
  authority produce relay plans; diagnostics shows the resulting reasons.

## The modularity principle — the core is tiny; protocol meaning is modular

The ownership table governs the API *surface*. A second principle governs *code weight*: **non-primitive, protocol-specific functionality is opt-in and modular, so an app carries only what it uses.**

The core is content-agnostic. Opt-in protocol modules own only the schemas,
validation, state reconstruction, semantic operations, and routing context of
their protocol. A module may provide a reusable closed binding such as
`myFollows`, but core does not bless a feed built from it. A NIP-29 group may
add group context to a foreign draft without taking ownership of its kind. See
*[Protocol modules, reusable declarations, and app policy](27-recipes-and-choosing.md)*.

---

<!-- nav-footer -->
<sub>← [Timeline in 10 minutes](04-ten-minute-timeline.md) · [Index](README.md) · [Your first app in 20 lines](06-first-app.md) →</sub>
