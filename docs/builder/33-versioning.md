# Versioning & stability: what "provisional-until-v2" means for you

**Status: BUILT** — this chapter describes the project's actual, current stability posture, which is recorded in [`README.md`](../../README.md), [`docs/VISION.md`](../VISION.md), and the [bug-class ledger](../bug-class-ledger.md). The posture itself is real and in force today; what it *governs* (the public API) is still being built.

After this chapter you'll know exactly what stability promise NMP is and isn't making right now, what changes without notice before v2, what stays fixed even in this provisional phase, and how to track breaking changes if you're betting an app on NMP today.

## The one-sentence posture

**Everything in NMP is provisional until a v2.0 ships (not before Aug 2026); nothing is self-compatibility-binding before then.** The engine is a day-0 greenfield rebuild whose entire premise — that a two-noun surface can hold correctness by shape — is still being proven milestone by milestone on real apps. Until that proof completes, the public API is deliberately unfrozen, so it can be cut *right* rather than cut early and grandfathered.

If you're used to semantic-versioning promises, read this as: **NMP is pre-1.0 in the strongest sense.** There is no compat obligation yet, on purpose.

## What "no self-compat obligation" actually means for you

Concretely, before v2.0:

- **Public types can change shape.** `NMPFilter`, `WriteIntent`, `Row`, `Coverage`, the `WriteStatus` cases, the diagnostics rows — any of these can gain, lose, or rename fields between commits. They are values defined once at the FFI seam and mirrored per platform; when the seam changes, every platform changes together.
- **Methods can be renamed or re-signed.** `observe`, `publish`, `setActiveAccount`, `addAccount` are the surface today; their names and signatures are not frozen.
- **Recipes can be deprecated freely.** A recipe is, by design, a deletable function (boundary test 5) — the cheapest thing to change. Do not treat any recipe as a stable contract before v2.
- **PLANNED surfaces will change most.** Anything this manual marks PLANNED — the [Collection observation mode](12-collection-mode.md), [delivery-side transforms](13-delivery-transforms.md), NIP modules and the [recipe layer](27-recipes-and-choosing.md), the Kotlin/TS SDKs — is a design preview. Its shape *will* move before it ships.

The flip side, and the reason this is safe to build on anyway: the churn is bounded to the *surface*, not the *guarantees*.

## What stays fixed even now

Some things are stable in spirit even in the provisional phase, because they're the bet itself rather than an implementation detail:

- **The two nouns.** A live query you observe and a write intent you publish. This is the thesis; it doesn't move without invalidating the project. There will never be a third app-facing noun (a "session," a "resource," a "module" beside the two) — that's a tripwire, not a roadmap item.
- **The bug-class ledger guarantees.** Each entry is append-only in spirit and defended by a CI falsification test. An API change that *erodes* a guarantee (say, adding a `Vec<RelayUrl>` route-override anywhere) is a **red build**, not a quiet regression. The ledger is a second truth anchor precisely so stability of *correctness* doesn't depend on stability of *syntax*. So while `observe`'s signature might change, "there is no `relays:` parameter" ([#3](28-patterns.md)) will not — the mechanism is protected even as the surface moves.
- **Values in, code after.** Anything the engine routes/keys/orders stays a closed, introspectable value; app closures stay after delivery. This governing rule outlives any specific type shape.
- **Library, not framework.** The M5 kill condition (no NMP-shaped scaffolding) is a permanent design rule, not a version.

So the honest summary: **the names and shapes are provisional; the nouns, the guarantees, and the ownership line are the stable core** even before v2 formalizes them.

## The truth-anchor discipline (why the docs won't mislead you)

NMP's documents never claim a mechanism holds before a test proves it. Every chapter of this manual carries a `BUILT` / `PARTIAL` / `PLANNED` banner; the bug-class ledger marks each entry with its CI proof status (and leaves it `not yet` until the falsification test is green); [`docs/known-gaps.md`](../known-gaps.md) lists built-but-incomplete and deliberately-deferred work so nothing hides. This discipline *is* the stability story during the provisional phase: you can't rely on a frozen API yet, but you can rely on the docs telling you exactly what is and isn't real. When something is marked BUILT with a running proof, betting on it is reasonable; when it's PLANNED, treat it as directional.

## How to track breaking changes if you're betting on NMP today

If you're building on NMP before v2, here's the practical protocol:

1. **Pin, then move deliberately.** Pin a specific commit/tag of the engine and SDK rather than tracking a moving branch, so a surface change never surprises a build. Upgrade on your schedule, reading the diff.
2. **Watch the ledger and known-gaps, not just the API.** A change to `docs/bug-class-ledger.md` (a new entry, or a proof-status change) tells you a guarantee moved or firmed up — more consequential than a renamed method. A change to `docs/known-gaps.md` tells you a rough edge closed or opened.
3. **Lean on the banners.** Build load-bearing features on `BUILT` surfaces with running proofs (today: the Swift and Rust SDKs, the two-noun read/write/identity/diagnostics loop). Prototype against `PLANNED` surfaces knowing they'll move.
4. **Keep your recipes yours until modules stabilize.** Because the recipe layer and module mechanism are PLANNED, write app-owned recipes (as the Falsifier's `FeedFilters` does) rather than depending on a not-yet-shipped module API. When modules stabilize, migrating an app-owned recipe to a module recipe is a mechanical change — the *value* it returns won't move even if where it lives does.
5. **Expect the SDK shape to get a Tier-A cut once.** The public SDK shape is an expensive-to-re-cut, product-facing surface; it gets an adversarial propose/refute round (Tier A) before it's considered settled. That's the moment the API stops moving casually — until then, assume it can.

## What v2.0 will change about all this

When v2.0 ships (not before Aug 2026), the posture inverts: the public surface that survived the milestones becomes a compatibility contract, and *then* normal deprecation discipline applies — breaking changes get versioned, recipes get deprecation windows, the grammar and nouns become genuinely frozen. The provisional phase exists to earn that freeze by proving the surface on real apps first, rather than freezing a guess. Until then, the deal is simple and stated honestly: **unfrozen surface, protected guarantees, truth-anchored docs.**

## What to read next

- *[What NMP does not do](29-not-do.md)* — the scope line, which is itself provisional-until-v2 but stable in spirit.
- *[Patterns & anti-patterns](28-patterns.md)* — the guarantees that stay fixed even while the surface moves.
- [`docs/known-gaps.md`](../known-gaps.md) and [`docs/bug-class-ledger.md`](../bug-class-ledger.md) — the two files to watch for consequential change.

---

<!-- nav-footer -->
<sub>← [Extending NMP](32-extending.md) · [Index](README.md)</sub>
