# Extending NMP: protocol modules & recipes

**Status: PLANNED** — this chapter describes the *intended shape* of the extension path. The engine core (the two nouns + hard concerns) is BUILT; the **module mechanism** (per-NIP crate / Cargo feature / registerable module) and the **recipe layer** it ships are not built yet. Where a mechanism is undecided, this chapter says so. Read it as directional agreement, not shipped API.

After this chapter you'll know the primary way to extend NMP — adding a protocol module — and the secondary way — adding a recipe — and you'll know the two rules every extension must satisfy: the five boundary tests and the modularity principle.

## The one thing to internalize first

You extend NMP by **adding a module that ships new protocol facts, not by widening the core**. The two nouns are forever; a new NIP's kinds, tags, recipes, and parsing are opt-in weight an app links only if it enables them. Extending the *grammar* or the *noun surface* is a different, far rarer, far more expensive act (a Tier-A design event) — this chapter is about the common case: teaching NMP about a new protocol without touching the core at all.

## The two rules, together

Every extension is governed by two principles that work in tandem:

**The boundary principle** governs the *surface* — what may cross into the engine's decision path. A new capability is blessed only if it passes all five tests: (1) desugars to public values; (2) encodes a protocol fact, not a product decision; (3) a second unrelated app would write it identically; (4) never the only door — the wrapped primitive stays public and its expansion prints; (5) deletable without breaking a [bug-ledger](28-patterns.md) guarantee. See [The batteries: recipes, and choosing](27-recipes-and-choosing.md) for the tree.

**The modularity principle** governs *code weight* — where the extension lives. Non-primitive protocol functionality is opt-in and modular, so an app carries only what it uses. Reactions, reposts, highlights, long-form, follow packs, lists, comments each live in their own module. A minimal app that never reacts links **zero** reaction code; adding follow-pack support must not tax every other app.

The boundary principle says *whether* something can be a convenience. The modularity principle says *where* it goes. Together: a legitimate protocol convenience is a value-returning recipe (boundary) that ships in its NIP's opt-in module (modularity).

## Adding a protocol module (the primary path)

A protocol module packages everything an app needs to speak one NIP, as opt-in weight. Its intended contents:

- **The kinds and tag conventions** the NIP defines (e.g. NIP-25 = kind:7, `e`/`p` target tags).
- **Read recipes** — value-returning `NMPFilter` builders for the NIP's query shapes (`.reactions(to:)`, `.thread(root:)`), each desugaring to the public grammar and printing its expansion.
- **Write recipes** — `WriteIntent` builders that fill kind/tags per the NIP, leaving durability and routing visible to the caller.
- **Parsing helpers** — pure functions turning delivered raw rows into the NIP's semantic shape (e.g. "this kind:7's target event id"), applied *after* delivery, never parameterizing engine behavior.

**The intended shape (illustrative — mechanism not final):**

```
// PLANNED — one of: a per-NIP crate, a Cargo feature on a protocol crate,
// or a registerable module. The PRINCIPLE is fixed; the MECHANISM is not.

// Enable the module (Swift, via SwiftPM product; Rust, via a feature/crate):
//   .product(name: "NMPReactions", package: "nmp")      // opt-in
// and its recipes appear:
let toEvent = NMPFilter.reactions(to: someEventId)        // from NMPReactions
// Don't enable it, and `.reactions(to:)` does not exist and no kind:7
// knowledge is linked into your binary.
```

Every module is subject to the five tests *per member*. A module may not smuggle in a member that takes an app closure over engine behavior, or that bakes a product decision (ordering, display), just because the module as a whole is about a real NIP. `.thread(root:)` ships the NIP-10 *membership* filter; it does not ship reply ranking or nesting, because those differ per app.

**What a module may NOT do**, no matter how "protocol-shaped" it looks:

- Register a closure as a lane-mapper, comparator, or admission predicate — this exact shape ("protocol modules register closure lane-mappings") was formally rejected as the old feed framework reborn. Values in, code after.
- Add a `relays:` parameter or any relay-picking convenience ([ledger #3](28-patterns.md)).
- Add display/formatting of any kind ([ledger #12](28-patterns.md)).
- Grow its own relay opinions or become a required import.

**The Blossom exception, understood correctly.** A media module needs an HTTP client, hash verification, server-list fallback, and signed auth events — real correctness machinery, more than "pure composition." It's still allowed, as a separate opt-in package that *consumes the engine's signer capability through a public seam* and uses the two nouns for server-list discovery (kind:10063 is just a query). It is engine-*adjacent*, not a new noun, and never a required import. That's the ceiling for how much a module may exceed pure sugar.

## Adding a recipe (the secondary path)

If the module already exists and you just want one more convenience, adding a recipe is the lightweight case. Walk the five tests, then write a pure function returning a public value:

```swift
// A recipe is a pure function to a public value. Nothing more.
extension NMPFilter {
    /// NIP-25 reactions targeting `eventId`. Desugars to a public value;
    /// prints its expansion; deletable (callers inline the value).
    static func reactions(to eventId: String) -> NMPFilter {
        NMPFilter(kinds: [7], tags: ["e": .literal([eventId])])
    }
}
```

Checklist for a recipe you propose:
1. **Desugars to public values?** It returns an `NMPFilter`/`WriteIntent` built only from the public surface. Could it live in a third-party package with zero privileged access? If not, it's mechanism, not a recipe.
2. **Protocol fact, not product decision?** Kinds/tags/NIP shapes — yes. "What my feed contains" — no.
3. **Identical in a second unrelated app?** If the parameter list or body would differ, it's under-general or it's policy.
4. **Wrapped primitive stays public and the expansion prints?** A caller can always ask "what did this build?" and get the value.
5. **Deletable?** Removing it breaks no engine invariant and no ledger entry — worst case, callers inline the value.

Pass all five and it belongs in the relevant NIP module. Fail test 1 → it's engine mechanism, and a new mechanism is a Tier-A surface change, not a recipe. Fail test 2 or 3 → it's your app's code (fold delivered rows with arbitrary logic — see [Delivery-side transforms](13-delivery-transforms.md)).

## When your need doesn't fit a recipe or module at all

Sometimes the honest answer is "extend the grammar" — a genuinely new read shape the closed `Selector` vocabulary can't express even after adding a vocabulary member. That is **not** a module or a recipe; it is a **Tier-A design event**: an adversarial propose/refute round with a human tie-break, because the grammar and the nouns are forever and changing them is the expensive category. The M0 gate did exactly this to add `SetOp` (without set-difference, "follows minus mutes" was inexpressible, contradicting [ledger #11](28-patterns.md)). If you think you've found such a case, you're not writing a module — you're proposing a grammar change, and the bar is deliberately high. Most needs are not this; they're a recipe or a delivery-side transform in disguise.

## Why this keeps the core small forever

The whole point of the module path is that it lets the ecosystem grow *without* growing the thing that must stay stable. New NIPs arrive constantly; if each one widened the core, the two-noun surface would rot exactly the way the old 46-principle surface did. Instead: the grammar is a small, hashable, routable value language that the engine understands completely; every protocol is expressed *in* that language, in a module you opt into; and an app's binary — and its cognitive load — is proportional to the NIPs it actually uses. You pay for the NIPs you enable, and nothing else.

## What to read next

- *[The batteries: recipes, and choosing](27-recipes-and-choosing.md)* — the recipe catalog and the recipe-vs-compose-vs-own tree.
- *[What NMP does not do](29-not-do.md)* — the deferred surfaces (wallet, DMs, Marmot, Blossom) that return, if at all, as modules under exactly these rules.
- *[Patterns & anti-patterns](28-patterns.md)* — the guarantees a module must never erode.

---

<!-- nav-footer -->
<sub>← [Example gallery](31-gallery.md) · [Index](README.md) · [Versioning & stability](33-versioning.md) →</sub>
