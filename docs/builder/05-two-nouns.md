# The two nouns and the ownership table

**Status: BUILT** — both nouns run today in the Swift and Rust SDKs. The modularity mechanism that packages *non-primitive* protocol helpers (reactions, reposts, lists) as opt-in modules is **PLANNED** and marked as such below.

After this chapter you'll be able to name where any piece of an NMP app belongs — engine, your app, or the UI framework — without guessing, because the whole surface is two nouns and you'll know what each one owns.

---

## Everything is one of two nouns

NMP's public surface is deliberately tiny. There are exactly two things you *do*:

1. **A live query** — the read noun. A Nostr `Filter` whose field values may be reactive `Binding`s, handed to `observe`. It's a plain, hashable, serializable **value**. You get back a stream of row snapshots plus a coverage state.
2. **A write intent** — the write noun. An unsigned event template plus a durability class and a routing class, handed to `publish`. You get back a **receipt** whose status streams.

That's it. Everything else you might reach for is *not a third noun*:

- **Identity** is an *input* — `addAccount` + `setActiveAccount`. You state a fact; the engine derives everything account-shaped from it.
- **Capabilities** (signer, decrypt, AUTH policy) are *plug points* — objects the engine invokes at the right moment, that you configure but don't call.
- **Diagnostics** are a *projection* — a read-only view of what the other planes did.

If you ever feel you need a "session," a "feed manager," a "subscription object," or a "relay pool" as a first-class thing you own, stop: that instinct is the old client-framework fragmentation returning. The answer is always a query you observe, a write you intend, or configuration of the machinery that serves those two. When you think you need a third noun, you almost always need a differently-shaped *value* for one of the two you have.

## The read noun, concretely

Here is the read noun in Swift and in Rust. Same value, two dialects:

```swift
// Swift — $myFollows: kind:1 notes by whoever my kind:3 currently names.
let filter = NMPFilter(
    kinds: [1],
    authors: .derived(
        inner: NMPFilter(kinds: [3], authors: .reactive(.activePubkey)),
        project: .tag("p")
    ),
    limit: 200
)
for await batch in try engine.observe(filter) {
    render(batch.rows)          // your code, after delivery
    show(batch.coverage)        // Unknown vs CompleteUpTo(watermark)
}
```

```rust
// Rust — the identical value through the Handle.
let query = LiveQuery(Filter {
    kinds: Some(BTreeSet::from([1u16])),
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
let intent = WriteIntent(
    pubkey: myPubkey,
    createdAt: UInt64(Date().timeIntervalSince1970),
    kind: 1,
    content: "hello nostr",
    durability: .durable,          // vs .ephemeral / .atMostOnce
    routing: .authorOutbox         // vs .toInboxes([...]) / .privateNarrow([...])
)
let receipt = try await engine.publish(intent)
for await status in receipt.status {
    // .accepted → .signed → .routed → .sent(relay) → .acked(relay) ...
}
```

Note what you *cannot* express: you never pick a relay to publish to. `.authorOutbox` is a routing *class*, not a relay list — the engine resolves it through the same lane machinery your reads use. `.privateNarrow` carries a fixed fail-closed set that has no widen operation. (The receipt lattice gets its own chapter: *Writing: intents, receipts, and durability*.)

## The ownership table

This is the whole mental model on one page. For any feature, find the row.

| Concern | NMP owns (engine) | Your app owns | The UI framework owns |
|---|---|---|---|
| **Which queries exist, and when** | — | ✅ you build `NMPFilter` values and call `observe` | — |
| **Binding resolution** (`Derived`, `Reactive`, `SetOp`) | ✅ resolves in-engine, incrementally | — | — |
| **Relay routing** (outbox, lanes, fan-out cap, coalescing) | ✅ compiler output from lane-typed facts | ❌ *no `relays:` parameter exists* | — |
| **Sync** (negentropy, coverage watermarks) | ✅ | — | — |
| **Row delivery + coverage state** | ✅ delivers raw-token rows + `Coverage` | ✅ folds them into your view state, arbitrary code | — |
| **Ordering / sorting** | ❌ delivers a live set, no order | ✅ ordering is render policy | — |
| **Formatting** (hex→npub, dates, kind:0 fields) | ❌ raw tokens only (ledger #12) | ✅ all of it, in app code | — |
| **Signing** | ✅ orchestrated via the signer capability | ✅ you supply *which* signer exists | — |
| **Write routing + durability + acks** | ✅ | ✅ you compose *what* to write and when | — |
| **Identity / re-rooting** | ✅ derives relay lists, outboxes, follow expansion | ✅ who is active; all login/onboarding UX | — |
| **Diagnostics numbers** | ✅ every figure from real engine state | ✅ rendering the screen | ✅ list/section chrome |
| **View lifecycle, `@State`, teardown** | ✅ demand drops when the handle is released | ✅ where the engine object lives | ✅ `.task`, ARC, scene phase |

Two lines are worth memorizing because they catch the most people:

- **Ordering and formatting are yours.** The engine hands you a live set of raw-token rows. It is not being lazy — a blessed sort order or a blessed date format is *one app's product decision* pretending to be framework, and baking it in is exactly the bug that killed the v1 feed layer. See how the timeline chapter sorts by `createdAt` and shortens hex pubkeys in *app* code.
- **Relay choice is unrepresentable, not discouraged.** There is no `relays:` field to avoid. You configure two indexers at construction; every other relay is the engine's compiler output. The diagnostics screen shows you relays you never named.

## The modularity principle — the core is tiny; protocol meaning is modular

The ownership table governs the API *surface*. A second principle governs *code weight*: **non-primitive, protocol-specific functionality is opt-in and modular, so an app carries only what it uses.**

The engine **core** is the two nouns plus the hard concerns: store, routing/outbox, sync/negentropy, coverage, identity, diagnostics, and the capability seams. That's it. Everything protocol-specific and non-primitive — reactions, reposts, follow packs, highlights, long-form, NIP-51 lists, comments — is *not* in the core. A minimal timeline app that never reacts must link **zero** reaction code, and adding follow-pack support must not tax every other app.

This is why, in the timeline chapter, the `$myFollows` filter shape lived in *your* `FeedFilters.swift`, not in NMP. NMP exposes nothing named "follows" — only the general `NMPFilter`/`NMPBinding` algebra. A convenience like `.reactions(to:)` or `.follows(of:)` is a **recipe**: a pure, value-returning function that a second unrelated app would write byte-for-byte identically. Recipes are blessed only when they encode a *protocol fact* (the kind number, the tag shape) and never a *product decision* (what a feed contains, how it's ordered). And crucially — **a recipe ships in its own per-NIP module, not in core.** Enabling the module is how you get its recipes and kinds; not enabling it is how they stay absent from your binary.

> **PLANNED — the module mechanism.** Today, only the two nouns and the raw `NMPFilter` algebra are built; there is no shipped recipe module yet. The *intended shape* is a per-NIP crate or a Cargo feature flag on a protocol crate: enable it → its recipes (`.react()`, `.reactions(to:)`) and kind numbers appear; don't → they're not compiled in. The manual teaches the principle now (*you pay only for the NIPs you enable*) and will show the concrete mechanism once it ships. See *Extending NMP: protocol modules & recipes* for the design preview. What you can rely on **today**: write the shape you need as an app-side function over the two nouns, exactly as `FeedFilters` does.

The payoff is the same win the old NMP genuinely had: the crate that encoded what reactions *mean* was one apps could decline to pack. The two nouns stay forever; a recipe is a deprecable function in a module you chose to enable. That keeps the expensive, permanent category — the grammar and the nouns — as small as this chapter.

---

<!-- nav-footer -->
<sub>← [Timeline in 10 minutes](04-ten-minute-timeline.md) · [Index](README.md) · [Your first app in 20 lines](06-first-app.md) →</sub>
