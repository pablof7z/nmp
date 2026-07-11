# What NMP does NOT do (and why)

**Status: BUILT** (conceptual — the scope line described here is a design decision recorded in [`docs/VISION.md`](../VISION.md) and the [design guidelines](000-design-guidelines-and-toc.md), and is enforced by the shape of the surface, not by a checklist).

After this chapter you'll be able to tell, for any capability you're wondering about, which side of NMP's scope line it falls on — and why the line sits where it does. Knowing what a library *refuses* is as important as knowing what it does: it tells you what stays your job, forever, by design.

## The one-sentence fence

**NMP owns the part of the problem that is Nostr's — a local-first synchronizing replica plus the network correctness to keep it right — and nothing that is your app's.** Everything downstream of "the engine delivered these rows" is yours, and everything about how your app is *shaped* is yours. The refusals below are all corollaries of that one sentence.

## It does not own your app's architecture

There is no NMP app architecture, because there is no NMP app architecture to learn. This is the thesis, stated as a refusal:

- **No mandatory state container.** The Falsifier's `AppModel` is a plain `@Observable` class the app authored; `NMPEngine` is just a `let` property on it. NMP provides no base class, no required environment/provider wrapper, no engine-owned app object you must adopt.
- **No lifecycle to adopt.** No scene-phase hooks, no mandatory background task you must schedule, no module registration. One construction call; every further feature is a method on that object. Two calls for a small app, twenty for a full client, zero imposed architecture either way.
- **No opinion about your patterns.** MVVM, TCA, plain SwiftUI, Redux — the engine doesn't know or care. It holds no view model, no navigation, no store-of-stores.

Why: the previous design owned the whole application (actor, app-state, reducers, projections) and a podcast player that touched Nostr for one feature had to buy an entire way of architecting itself. The kill condition for v2 (M5, judged by a human on a real app) is precisely this: *if using NMP makes your app NMP-shaped, the design has failed.* The refusal is load-bearing, not incidental.

## It does not do presentation

The engine emits **raw tokens only**: hex pubkeys, Unix timestamps, verbatim kind:0 content. It ships:

- no display/formatting helpers,
- no truncation, no locale handling, no relative-time strings,
- no "fallback display name for a missing profile,"
- no formatted-string field on any FFI type.

This isn't an oversight you should route around; the vocabulary to express presentation is *absent from the engine* ([bug-ledger #12](28-patterns.md)). Your `shortHex()` and your date formatter live in your view, like the Falsifier's do. Why: baking one app's display choices into a shared layer is exactly the rot that hit the old repo 27 times. Presentation differs per app; a blessed answer would be one app's policy masquerading as framework.

## It does not make product decisions

The engine gives you *mechanism*; the *policy* is always yours:

- **What your feed contains** — you compose the queries.
- **How replies rank or nest** — the `.thread(root:)` recipe gives you NIP-10 *membership* and stops there; ordering and nesting are yours.
- **What counts as a conversation, a mention, a "home feed"** — product decisions, each app answers differently.
- **Which notes to hide** — mute/WoT scoring is your code, applied to *delivered* rows (see [Delivery-side transforms](13-delivery-transforms.md)), never a parameter to the engine.

The test for any convenience: would a second, unrelated app write it *byte-for-byte identically*? If the body would differ, it's product policy and NMP won't ship it. See [The batteries: recipes, and choosing](27-recipes-and-choosing.md) for the full boundary.

## It does not let you pick relays

There is no `relays:` parameter anywhere — not on reads, not on writes ([bug-ledger #3](28-patterns.md)). You supply your indexer set as operator *policy* at construction; from there, relay choice is the engine's compiler output. You cannot pin a query to a relay, cannot union relay lists, cannot override a route. Why: manual relay lists are the single richest source of outbox-era bugs, and the only way to make them unwritable is to remove the parameter. Routing is the engine's mission and it is not optional.

## It does not own your identity/session UX

Identity is a pure *input*: "the active signer is A… now B." NMP holds:

- no session model,
- no login flow,
- no onboarding, no account picker, no "remember me,"
- no account vector.

`addAccount` + `setActiveAccount(pubkey?)` is the entire contract. Everything *derived* from the active account (relay lists, outboxes, follow expansion, re-rooting) is the engine's; everything *around* it (which account is active, and all login/onboarding UX) is yours. See [Identity & multi-account](16-identity.md).

## What's explicitly out until v2 is proven

These are not "never" — they are *not yet*, deliberately deferred until the two-noun thesis is proven on real apps. Committing to them now would re-grow the wide surface v2 exists to avoid.

- **Wallets & zaps (NIP-60/61, NIP-57).** Out. A wallet is a product with its own trust model; it is not sync-and-routing machinery.
- **DMs (NIP-17 / gift-wrap).** The *feed* path is the proving ground; DMs are deferred. Today: DM inbox routing is known-incorrect (falls back to write relays and says so inline rather than shipping silent wrongness), and the decrypt-result feedback path is an explicit no-op. Both are tracked in [`docs/known-gaps.md`](../known-gaps.md), both off the falsifier's feed path.
- **Marmot / MLS group chat.** Out. Group-chat cryptography is an entire subsystem with its own correctness story; it does not belong in the core bet.
- **Search beyond the reserved field.** The `search` filter field is reserved in the grammar, but the NIP-50 lane is PLANNED, not built.
- **Blossom media.** PLANNED as an *opt-in module* (a signer-capability consumer + two-noun discovery), never as core and never a required import.

The rule for all of these: they return as **opt-in modules** under the [five boundary tests](27-recipes-and-choosing.md) and the [modularity principle](32-extending.md) — an app that never sends a zap links zero zap code — or they don't return at all. They will not return by widening the two-noun core.

## The refusals are a promise, not a limitation

Read positively, every refusal above is a guarantee about what stays stable and yours:

- Your app's shape is yours — NMP will never demand you restructure it.
- Your presentation is yours — the engine will never surprise you with a formatted string you have to un-format.
- Your product is yours — the engine will never bake in a feed policy you then have to fight.
- Your relays are the engine's — you will never debug a routing bug you caused by picking one.

The line between "engine mechanism" and "app policy" is drawn so the hard bugs can't cross it, and so the two nouns stay small enough to hold forever. When you're unsure which side a request falls on, ask the [five boundary tests](27-recipes-and-choosing.md); the answer they give is the answer NMP gives.

## What to read next

- *[The batteries: recipes, and choosing](27-recipes-and-choosing.md)* — where the boundary admits a convenience and where it refuses one.
- *[Extending NMP](32-extending.md)* — how deferred surfaces come back as opt-in modules without taxing every app.
- *[Versioning & stability](33-versioning.md)* — what "provisional-until-v2" means for the scope line itself.

---

<!-- nav-footer -->
<sub>← [Patterns & anti-patterns](28-patterns.md) · [Index](README.md) · [Platform SDK guides](30-platform-guides.md) →</sub>
