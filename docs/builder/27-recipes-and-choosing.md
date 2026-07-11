# The batteries: recipes, and choosing recipe vs compose vs own

**Status: PARTIAL** — the grammar every recipe desugars to is BUILT (you can write all of these by hand today, and the Falsifier does). The *named recipe layer itself* and the *per-NIP modules* it ships in are PLANNED: this chapter shows the intended shape, clearly marked, and shows the real hand-written values each recipe is sugar for.

After this chapter you'll know what a "battery" is in NMP (a thin, value-returning shortcut over the two nouns), why every recipe ships in its NIP's opt-in module rather than in core, and how to decide — for any given need — whether to reach for a recipe, compose primitives yourself, or own the whole thing in your app.

## What a recipe is (and is not)

A recipe is a pure function that returns one of the public values you already know: an `NMPFilter` (a live query) or a `WriteIntent`. Nothing more. It encodes a *protocol fact* — a kind number, a tag convention, a NIP-defined filter shape — so you don't have to remember it. It is not a new noun, not a new capability, and not a privileged path into the engine.

The litmus, which you should apply to any battery you're offered or tempted to write: **could this exact function live in a third-party package with zero special access to the engine?** If yes, it's a legitimate recipe. If it needs a private engine hook, it's mechanism (and belongs behind the surface). If two unrelated apps would write its *body* differently, it's product policy (and belongs in your app, not in NMP).

Because a recipe returns a value, everything the engine does to a hand-built value it also does to a recipe's output: hashing, dedup, coalescing, routing, coverage. A recipe cannot smuggle in a bug the grammar forbids, because it *is* the grammar.

## The crucial rule: recipes live in the NIP's module, not in core

This is the load-bearing constraint of this chapter, and it is the [modularity principle](32-extending.md) in action.

> **A recipe for a NIP ships in that NIP's opt-in module. Enabling the module is how you get the recipe. An app that never reacts links zero reaction code.**

Core is the two nouns plus the hard concerns (store, routing, sync, coverage, identity, diagnostics, the capability seams). Reactions, reposts, highlights, long-form, follow packs, comments, lists — everything protocol-specific and non-primitive — lives in its own module. `.reactions(to:)` is not a method on the engine; it appears *because you enabled the reactions module*. A minimal reader that shows kind:1 notes and nothing else carries no reaction code, no repost code, no list code. Adding NIP-51 lists to your app must not tax an app that never touches them.

This was the previous NMP's one genuine win worth keeping: the reactions NIP crate encoded what reactions *mean*, and apps that didn't care never packed `.react()`. NMP v2 keeps that shape deliberately.

**Where this stands today:** the module mechanism (per-NIP crate / Cargo feature / registerable module) is not built yet — see [Extending NMP](32-extending.md). Today the engine core ships the general `NMPFilter`/`NMPBinding` algebra and nothing named "follows" or "reaction." The Falsifier proves the point from the other direction: it writes its *own* `follows(kinds:)` recipe app-side (`FeedFilters.swift`), because NMP core deliberately exposes no such helper. That app-owned function is exactly the shape a blessed recipe takes — the only open question is *where it lives* (your app, a community package, or an official NIP module), never *whether it's allowed to exist*.

## The catalog (intended shape)

Each recipe below is shown as the value it desugars to — the real, current grammar. Every recipe can **print its expansion**: you can always ask "what filter did this actually build?" and get the value back, because the wrapped primitive stays public (boundary test 4). None of these is ever the *only* door to what it wraps.

### Read recipes (`NMPFilter`)

**`.profile(of: pubkey)`** — kind:0 for one author. Module: core-adjacent identity NIP.
```swift
// .profile(of: "3bf0…459d") desugars to:
NMPFilter(kinds: [0], authors: .literal(["3bf0…459d"]))
```

**`.follows(of: binding)`** — the kind:3 → `Tag(p)` derivation. Module: NIP-02.
```swift
// .follows(of: .reactive(.activePubkey)) desugars to:
.derived(
    inner: NMPFilter(kinds: [3], authors: .reactive(.activePubkey)),
    project: .tag("p")
)
// This is a *binding* (an author set), used as a filter field:
NMPFilter(kinds: [1], authors: .follows(of: .reactive(.activePubkey)), limit: 200)
```
This is the Falsifier's `FeedFilters.follows(kinds:)` verbatim — a recipe in everything but its shipping location.

**`.reactions(to: eventId)`** — reactions targeting one event. Module: NIP-25 (opt-in).
```swift
// .reactions(to: "abcd…") desugars to:
NMPFilter(kinds: [7], tags: ["e": .literal(["abcd…"])])
```
Enable the NIP-25 module and this appears; don't, and your binary carries no kind:7 knowledge.

**`.thread(root: eventId)`** — the NIP-10 filter *membership* shape (events tagging the root). Module: NIP-10.
```swift
// .thread(root: "abcd…") desugars to:
NMPFilter(kinds: [1], tags: ["e": .literal(["abcd…"])])
```
Note what this recipe is *not*: it is membership only. It does not order replies, nest them, or decide "what counts as a conversation." Those are product decisions each app answers differently, so they never ship as a recipe (boundary test 2).

**`followsMinusMutes(of: binding)`** — named compound over `SetOp`. Module: NIP-02 + NIP-51.
```swift
// desugars to:
.setOp(.diff, [
    .follows(of: .reactive(.activePubkey)),                       // NIP-02
    .derived(inner: NMPFilter(kinds: [10_000], authors: .reactive(.activePubkey)),
             project: .tag("p"))                                  // NIP-51 mute list
])
```

### Write recipes (`WriteIntent`)

**`.textNote(content:)`** — a kind:1 note. Module: core-adjacent.
```swift
// .textNote(content: "gm") desugars to (durability/routing stay VISIBLE):
WriteIntent(pubkey: me, createdAt: now, kind: 1, tags: [],
            content: "gm", durability: .durable, routing: .authorOutbox)
```

**`.reaction(to: event, content: "+")`** — a kind:7 reaction. Module: NIP-25.
```swift
WriteIntent(pubkey: me, createdAt: now, kind: 7,
            tags: [["e", event.id], ["p", event.pubkey]],
            content: "+", durability: .durable, routing: .authorOutbox)
```

A write recipe fills kind and tags per the NIP; it never hides durability or routing — you still choose those, because they are correctness-bearing properties, not protocol trivia (see [Writing](14-writing.md)).

### What is NOT a recipe, ever

- **Anything with ordering, windowing, cursors, or row identity.** That is the [Collection observation mode](12-collection-mode.md)'s closed vocabulary — cursor correctness can't ride on a helper (candidate ledger #13).
- **A one-call `follow()` that mutates your contact list.** The read-modify-write "wiped my follows" bug (see [Editing replaceable state safely](15-editing-replaceable.md)) would return in blessed packaging. NMP ships the *filter and template* recipes but not the stale-state-blind mutation.
- **Any display/formatting helper** (ledger #12): the vocabulary for presentation is absent from the engine and the recipe layer does not re-add it.
- **Any relay-picking convenience.** There is no `relays:` parameter (ledger #3) and no recipe may reintroduce one.

## Choosing: recipe vs compose vs own

Here is the decision tree. Walk it top to bottom for any need.

**1. Is it a pure protocol fact — kinds, tags, a NIP-defined filter shape?**
   - Yes → **use a recipe** (enable the NIP module), or write the recipe yourself if none exists. It should desugar to a value and print its expansion.
   - No → go to 2.

**2. Would a second, unrelated app write this *identically*, down to the parameter list?**
   - Yes, but there's no recipe for it → **compose primitives** yourself from `NMPFilter`/`NMPBinding`/`WriteIntent`, and consider contributing it as a recipe. This is the Falsifier's position: it composes `follows(kinds:)` from the raw algebra because the sugar doesn't exist yet.
   - No — the *body* would differ between apps → go to 3.

**3. Is it a product decision — what your feed contains, how replies rank, what a mention renders as, which notes to mute?**
   - Yes → **own it.** This is your app's job. Fold delivered rows into your own state and apply arbitrary code *after* delivery (see [Delivery-side transforms](13-delivery-transforms.md)). NMP will never bless an answer here, because a blessed answer would be one app's policy pretending to be framework — exactly the line the previous design's feed layer died on.

The five boundary tests are the same tree stated formally. A candidate battery must pass **all five**: (1) desugars to public values; (2) protocol fact, not product decision; (3) a second unrelated app writes it identically; (4) never the only door — the primitive stays public and the expansion prints; (5) deletable without breaking any [bug-ledger](28-patterns.md) guarantee. Fail any one and it's not a recipe — it's either engine mechanism or your app's code.

## Why this keeps feeling batteries-included anyway

The promise of "your first app in 20 lines" is delivered by *recipes over a small grammar*, not by a big surface. TanStack Query won the same way: a tiny core contract, an ecosystem of thin value-level conveniences, and nobody builds "a TanStack Query app." A recipe you never enable costs you nothing — not binary size, not surface area, not a correctness seam. A recipe you do enable is a deprecable function, never a permanent widening of the grammar. That asymmetry — grammar forever, recipes disposable — is what keeps the expensive category small while the convenient category grows freely.

## What to read next

- *[Extending NMP](32-extending.md)* — how a new NIP module (and its recipes) gets added under the five tests and the modularity rule.
- *[Patterns & anti-patterns](28-patterns.md)* — the guarantees every recipe inherits for free.
- *[Live queries & the binding grammar](09-binding-grammar.md)* — the algebra all read recipes desugar to.

---

<!-- nav-footer -->
<sub>← [Troubleshooting & FAQ](26-troubleshooting.md) · [Index](README.md) · [Patterns & anti-patterns](28-patterns.md) →</sub>
