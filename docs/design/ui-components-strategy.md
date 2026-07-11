# UI components across platforms — strategy design note

- **Date:** 2026-07-11
- **Status:** Ideation note for an owner decision. Nothing here is v2-blocking; VISION §3 already classifies the "optional UI component package" as north-star, not gate. This note decides what shape that north star may legally take — and names the parts of the ask that cannot be fully satisfied without breaking the thesis.
- **Anchors:** VISION P1 (library, not framework), §3 ownership table (rendering/layout/navigation belong to the UI framework), §10 ("values in, code after"; Collection observation mode), bug-ledger #11 (no app-owned expansion) and #12 (no presentation in core); README "Who owns what."
- **Cautionary ground truth (old repo, `nostr-multi-platform`):** `crates/nmp-component-registry/` (122 files, 5 platform targets), `crates/nmp-content/src/{tokenizer,grouper}.rs`, `docs/recipes/content-rendering.md`, `docs/recipes/app-shapes.md`, `docs/cli.md` §"add component"; the three independent consumer-app forks of `NostrInlineVideoPlayer.swift` (evidence in §2.3).

---

## 1. The problem, honestly stated

The owner wants NMP to offer UI components so that app builders don't re-implement the note card / thread view / profile header / compose box on every platform. That is a real product goal, and it is the single most framework-shaped thing this project could do. The old repo tried it, and its component story is one of the clearest specimens of how "just helpers" re-annexed the app: this note's first job is to extract *why* it crept, so the recommendation is grounded in mechanism, not vibes.

The four constraints this design must hold simultaneously:

1. **Library, not framework (P1).** The README assigns rendering, layout, navigation to the UI framework. A component story must not silently re-annex them.
2. **Two nouns + "values in, code after."** Anything the engine uses to *decide* is a closed introspectable value; app closures may fold *delivered* rows into view state but never parameterize engine behavior. "Rendering a note" must land cleanly on one side of that line.
3. **Modularity.** A minimal app links zero of any component offering.
4. **Multi-platform reality.** SwiftUI, Compose, and web are different rendering systems. Pixels do not port. The design must be honest about what is genuinely shareable.

## 2. What happened last time — the mechanism of the creep

The old repo's component registry was, on paper, the *right* instinct: shadcn-style copy-in source components (`nmp add component swiftui/content-view`), app-owned after install, "pure renderers — they do not fetch, retry, cache, route, or decide policy" (`docs/cli.md`). It crept into a framework anyway, through five distinct mechanisms. Each one is a rule for v2.

### 2.1 Components consumed engine-interior seams, not the app-facing surface

`NostrContentView` did not consume "what any app would." Its host contract (`docs/recipes/content-rendering.md` §"Host Contract") required `refs.profile`, `refs.event`, and `refs.event.envelopes` — engine-interior projections — plus an `EventRefResolverProtocol` forwarding visible refs into `resolve_ref`/`release_ref`. Installing a "content view" therefore *obligated the app into the projection/registry architecture*: an app-root `NmpComponentHost(profileHost:embedSource:eventRefResolver:kindRegistry:)` provider (`crates/nmp-component-registry/registry/swiftui/component-host/NmpComponentHost.swift`), concrete bridge objects, and a resolve/release lifecycle the component "managed" on the app's behalf. That is precisely M5's kill phrase — *NMP-shaped scaffolding* — imported through the side door of a view.

### 2.2 Extension by registration, not composition

Customizing one card meant registering a renderer into a `NostrKindRegistry` dispatch table (`registry.setArticle(MagazineArticleRenderer())`, content-rendering.md Recipe 4). Inversion of control: the component owns the dispatch loop and calls the app. Radix, shadcn, and every headless library that has stayed a library extend by *composition* — you pass children/slots; nothing registers into anything. A registry is a framework's spine wearing a component's clothes. (The SwiftUI registry alone grew 23 component families — `content-kind-0`, `content-kind-9802`, `content-kind-30023`, `chat-composer`, `login-block`, `user-card`, … — each new kind another registry entry, another dispatch arm, another doc.)

### 2.3 The AVPlayer saga — the shared component that wasn't shareably correct

The canonical registry component constructs a media player **inline in the SwiftUI body**:

```swift
// crates/nmp-component-registry/registry/swiftui/content-view/NostrContentView.swift:217
VideoPlayer(player: AVPlayer(url: first))
```

A new `AVPlayer` per body evaluation — full `AVPlayerViewController` KVO churn on every unrelated re-render of the containing note; observed to saturate the main thread for minutes on a video-bearing feed. Three external consumer apps each fixed it **independently**, and their fixes exist today as three files with three different md5 sums:

- `hl/app/ios/.../Vendor/nmp/Components/NostrContent/NostrInlineVideoPlayer.swift` — eager player in `@State` initialValue, plus a `failed`-latch so a dead URL isn't retried forever;
- `29er/ios/29er/29er/Components/NostrContent/NostrInlineVideoPlayer.swift` — near-copy of hl's, already drifted;
- `chirp/apps/ios/Chirp/Components/NostrContent/NostrInlineVideoPlayer.swift` — a *different design entirely*: lazy poster + tap-to-play, per chirp#63's acceptance criterion.

Meanwhile the canonical registry file **still ships the bug**. Two lessons, not one:

- **Copy-distribution without an upstream path means divergence-without-reconciliation.** Fixes flowed neither up nor across; "shared" was true only at install time.
- **Deeper: a stateful, resource-owning element has app-policy behavior, so a single canonical implementation is wrong for someone by construction.** chirp *wanted* lazy tap-to-play (scroll performance); hl wanted eager-with-latch. Neither is a bug fix of the other. Media playback, image prefetch, link-preview fetching — anything that owns a resource lifecycle is app policy and must be a *slot*, never a baked default with retrofitted knobs.

### 2.4 Policing app-owned code — the contradiction in terms

`docs/recipes/app-shapes.md:140`: "Do not fork `NostrContentView` or parse raw events in a renderer." A doc-level prohibition on modifying files the model explicitly hands the app to own. When your component story needs a police force patrolling the consumer's own source tree, the correctness didn't live in the shape — the exact failure the whole v2 thesis exists to escape.

### 2.5 What the old repo got *right* — and knew it

Two positive controls, both worth harvesting as principles:

- **The tokenizer** (`crates/nmp-content/src/tokenizer.rs`): one entry point, content string + tags → a `ContentTree` of typed segments (mentions, hashtags, links, invoices, custom emoji, markdown blocks). Pure, deterministic, closed vocabulary, raw tokens only. It is protocol *parsing* — that `nostr:npub1…` is a mention is a NIP-27 fact, not a rendering choice. This is the data side of the line done right, and every platform needs identical semantics of it.
- **The grouper knew where the line was** (`grouper.rs` header): "classification (whether a URL is media or generic) is a **rendering** concern, not a protocol one. Keeping the cut means apps that want raw URLs can skip the grouper." The old repo correctly ran the line *through* the middle of nmp-content and made the render-flavored pass separable.
- **Feed = mechanics, not render policy** (old #3082): `RootIndexed`/`Nip10ReplyAttribution` welded one app's render model into the feed engine; the settled ruling deleted them and established the owner principle — *"does any app USE it" is the wrong question; expose mechanics, don't bake one app's surface.* Same shape as display-separation (raw pubkeys only across FFI; `display::` banned from projections — enforced there by lint and audit, i.e. by policing; here it is ledger #12, enforced by absent vocabulary).

## 3. The line

Where "engine/data-owned content structure" ends and "app/UI-framework-owned rendering" begins, drawn as altitudes. Every candidate offering must name its layer.

| Layer | What it is | Sharing | Legality |
|---|---|---|---|
| **L0 — engine** | Events, rows, receipts, coverage, diagnostics. Raw tokens only: hex pubkeys, Unix timestamps, verbatim content (ledger #12). | The engine itself | Already settled. Nothing rendering-adjacent may ever live here. |
| **L1 — protocol-semantic data** | Pure functions from delivered rows/content to *values* with closed vocabularies: content token tree (NIP-21/27/30), thread assembly (NIP-10 reply tree), zap-receipt parse (NIP-57). No I/O, no engine parameterization, off the demand path. | **Genuinely cross-platform** — one Rust implementation, values across FFI | Legal and squarely inside "values in, code after": these fold *delivered* rows; the engine never consults them. The vocabulary names protocol facts (`Mention(hex)`, `Hashtag`, `Url`, `EventRef(id)`), **never visual structure** (no card/row/column/gallery nodes — that's where a token tree quietly becomes a render-IR). |
| **L2 — headless behavior (per platform)** | Thin observable view-models per concept — a composer model (draft state, mention autocomplete via an ordinary profile query, submit = write intent whose receipt stream it exposes), a thread model (a live query + L1 assembly). TanStack/Radix altitude. | Per-platform idiom (`@Observable` / `ViewModel`+`Flow`), but each is a ~200-line shim because **the hard behavior is already the engine**: liveness, pagination, ordering, windowing are the Collection observation mode (§10); durable submit is the write noun. | Legal *only if* built from the public SDK alone (rule R3 below). The moment one needs a private engine hook, the SDK surface is deficient — fix the surface, ledger-style. |
| **L3 — reference views (per platform)** | Actual pixels: note card, profile header, compose box, thread view, in SwiftUI / Compose. | **Not shareable.** N implementations, one per platform, sharing only their L1/L2 substrate. | Legal only as ordinary consumers of the two nouns "exactly as any app would" (VISION §3), packaged and governed per §5. |
| **App** | Screen composition, navigation, theming decisions, resource-lifecycle policy (media playback, prefetch), anything it replaces. | — | — |

The pivotal distinction, inherited from the old repo's late realization (its #3113: *codec vs formatting*): **codec** — mapping between protocol representations (hex↔npub, content→token tree, rows→reply tree) — is shared, reusable, one-implementation work. **Formatting** — truncation, locale dates, "2h ago", display-name fallback chains — is app-owned presentation and appears in *no* NMP layer, not even L3's reference views except as trivially replaceable view code. L1 is all codec; the engine stays all-raw; formatting never gets a shared home.

## 4. The design space

### Option A — Content tokenization as data (L1 only; "the tokenizer, nothing downstream of it")

Ship `nmp-content` v2: opt-in crate, `tokenize(content, tags, kind) -> ContentTree` of raw-token segments, exposed through the SDK as a plain value (and as a pure Swift/Kotlin function call — it doesn't even need to touch the engine handle). Rewrite through the import gate; the old tokenizer is a legitimate harvest candidate, its downstream registry is not. Media *grouping* stays out of the core vocabulary (the old grouper's own header justifies this) — emit individual media tokens; a platform kit may group.

- **Shared:** everything — one Rust implementation is the whole point; identical NIP-21/27/30 semantics on every platform is a correctness property, not a convenience.
- **Constraints:** honors all four. Opt-in crate = zero weight (C3). Values with closed vocabularies (C2). No rendering opinion whatsoever (C1). Fully portable because it's data (C4).
- **Failure mode:** vocabulary creep — segments that describe *appearance* (gallery, card, collapsed-run) rather than protocol facts. The tripwire is nameable: any variant whose meaning you'd explain by describing pixels is a render-IR node. Second failure: an `EventRef` token tempts a "resolver" abstraction back into existence (the old `refs.event.envelopes` chain). v2 answer: an event ref resolves by *the app running another live query* — the engine already dedups and caches; there is no third resolution machinery to build.

### Option B — Headless/behavior components (L2)

Per-platform packages of concept view-models. Honest inventory of what behavior actually remains once the engine exists: **less than TanStack's, by design.** Query lifecycle, caching, pagination, ordering, deltas, receipt state machines — all engine. What's left: draft/composition state, autocomplete orchestration, L1 folding, and platform-reactive packaging. Each headless component is thin — which is the *proof the engine's surface is right*, and the first place a gap would show (dogfooding, VISION §3's "second job").

- **Shared:** the shape and the L1 substrate; the code is per-platform idiom.
- **Constraints:** honors C1 iff zero-scaffold (drop into a bare view, no host/provider) and public-surface-only. C2: legal by construction — folds delivered rows, submits intents. C3: separate packages, trivially zero-weight. C4: honest — behavior semantics port, code doesn't.
- **Failure mode:** the soft framework. A family of `Nmp*Model` types becomes "the NMP way to build screens," and docs start assuming them. Mitigation is structural (§5 R3/R4) plus a docs rule: every L2 component's documentation *shows the raw two-noun equivalent first*, the component second.

### Option C — Per-platform reference component kits (L3, opt-in, outside the engine contract)

SwiftUI package + Compose artifact of actual note-card/thread/profile-header/compose views, consuming L1+L2. This is the part the owner is actually asking for, and the part the old repo demonstrated is dangerous. It is *legal* under the constraints only with the §5 rules; without them it re-runs §2 beat for beat.

- **Shared:** nothing at the pixel layer. What's shared is that both kits sit on the same L1 values and the same public SDK — so their *correctness* is shared even though their code isn't.
- **Constraints:** C1 is the knife-edge — held only by the structural rules below. C2: fine (render side). C3: separate packages. C4: honest — this option openly commits NMP to N parallel UI codebases (the old registry's 122 files across 5 targets is the price tag from last time; budget for 2 targets max, ever).
- **Failure modes,** each mapped to its §2 ancestor: components needing engine-interior seams (§2.1) → R1/R4; kind-dispatch registries (§2.2) → R5; baked resource policy (§2.3) → R6; policing app copies (§2.4) → distribution choice in §5.
- **Distribution:** linked packages (SwiftPM/Maven) as the default, *reversing* the old copy-in model. The three-fork AVPlayer evidence shows copy-in failed both directions: the canonical stayed buggy, the forks diverged. With linked+slots, a bug fix ships once, and the legitimate per-app divergence (eager vs lazy playback) lives in slots rather than forks. Vendoring the source remains trivially possible for an app that wants full ownership — but NMP builds no copy/lock/update machinery (`nmp add component`, `nmp.components.lock`, hash-diff updates were themselves framework tooling; never rebuild them).

### Option D — Cross-platform render-IR (rejected, and worth rejecting precisely)

A Rust-side declarative UI description (nodes like card/column/text-style) that platform interpreters render. Rejected on all four constraints at once: it *is* a UI framework (C1 — rendering and layout re-annexed wholesale); its node vocabulary is exactly the "visual structure" a closed value must not encode (C2 in spirit); it is mandatory weight for anything rendered through it (C3); and it lands on lowest-common-denominator or grows per-platform escape hatches, the two classic ends of every write-once-render-anywhere attempt (C4). The old repo's `ContentTreeWire` was only a *mild* IR — semantic tokens, no layout nodes — and even it pulled kind-registries, dispatchers, and hosts into existence around it. A real IR is that gravitational field squared. The one thing to keep from considering it: the discipline it clarifies — **the shareable cross-platform artifact is what content *is*, never what it *looks like*.**

## 5. Recommended direction

**A layered offering: Option A now, Option B thin, Option C later and governed — bound by six structural rules.** NMP's answer to "don't make every app rebuild the note card" is: share 100% of the semantics (L1), share the behavior shape (L2), and offer per-platform reference pixels (L3) as ordinary, replaceable consumers — while the engine remains provably ignorant that any of it exists.

The rules — stated ledger-style, structural and falsifiable, not lints:

- **R1 — Public-surface-only.** L2/L3 packages link the *published* SDK artifact. They cannot import engine internals because internals are not in their dependency graph — enforced by packaging, not review.
- **R2 — Zero-scaffold.** Every component drops into a bare SwiftUI view / Compose function with only its declared inputs. No app-root host, provider, or environment installation may be *required*. (Native environment use for optional theming is platform idiom, fine.) Falsifier: a one-file sample app per component, compiled in CI.
- **R3 — Rederivability.** Every reference component must be writable by an app developer from the public API alone. A component that needs a private hook is a *discovered SDK deficiency*: fix the surface first, then the component consumes the fix publicly. This makes the kit a permanent dogfooding instrument — the M5 question ("library or framework in disguise?") re-asked continuously.
- **R4 — Engine blindness.** The engine repo contains no component registry, no component CLI verb, no component-aware type. The dependency arrow points one way. (Sharpest form: L3 kits live in sibling repos — see owner decisions.)
- **R5 — Composition, not registration.** Extension = slots / children / native composition. No kind-dispatch registry, no renderer registration, no inversion of control. An app that wants a different article card passes a different view; nothing is "installed."
- **R6 — Policy is a slot.** Anything owning a resource lifecycle (media playback, image loading, link-preview fetch) is a slot with a deliberately minimal default (static placeholder + tap-out). The AVPlayer rule: a canonical stateful default is wrong for someone by construction.

**What NMP owns / offers optionally / leaves to the app:**

| Owns (engine, always) | Offers optionally (opt-in, replaceable) | Leaves to the app |
|---|---|---|
| Rows, receipts, coverage, ordering/windowing (Collection), diagnostics — raw tokens only | L1 `nmp-content` token tree + thread/zap folds (crate/feature) | Screen composition, navigation |
| The correctness underneath every component | L2 headless concept models (per-platform package) | Formatting: truncation, locale, relative time, name-fallback chains |
| — | L3 reference view kits (per-platform, sibling-packaged, slot-composed) | Resource policy (what fills the slots); theming; every component it swaps out |

## 6. Staging

1. **Now / independent of M5:** nothing ships, but the L1 spec (token vocabulary, closed, protocol-facts-only) can be written and adversarially reviewed cheaply — it's a grammar question, the kind this repo is good at gating (Tier-A style propose/refute on the vocabulary).
2. **First shipped artifact — `nmp-content` v2 (L1).** Cheap, obviously legal, immediately useful, and the falsifier is its natural first consumer *after* the M5 verdict is rendered (don't add NMP-provided machinery to the falsifier while it's still the specimen under judgment).
3. **Gated on M5's verdict — L2, then L3.** The component question is strictly downstream of "is the library thesis true": reference components built on an unjudged SDK would contaminate the judgment (they'd *be* the scaffolding M5 exists to detect), and if M5's kill fires, there is no surface worth componentizing. First L3 target: SwiftUI note card + thread view, sibling repo, R1–R6 from day one, with the one-file R2 sample apps as its CI.
4. **Never:** copy/lock/update component tooling; kind registries; component hosts; a web/TUI/desktop kit before two platforms have proven the model (the old registry's five simultaneous targets were maintenance, not leverage).

## 7. Honesty — what cannot be reconciled, and the owner decisions

**The ask, taken literally, is not fully satisfiable.** "Offer UI components so builders don't re-implement per platform" implies shared component *code*. There is no such thing across SwiftUI/Compose/web without building the render-IR (Option D), which is the framework this rebuild exists to escape. What is actually on offer: NMP removes the *hard* 80% (parsing, threading, ordering, liveness, correctness — L0/L1/L2) once, cross-platform; the remaining pixels are re-implemented per platform *by NMP instead of by each app*. That is a **maintenance transfer, not a deduplication** — real product value (each app builder writes zero of it), but a permanent N-platform UI-maintenance business for this project, with a support surface (consumers filing visual bugs against the kit) and a soft-power risk (the reference kit becomes the de-facto "Nostr look," and its choices start reading as NMP policy). The old repo's 122-file, five-target registry is what that business costs when unbudgeted.

**The residual thesis tension that never fully dissolves:** any official component kit, however governed, exerts gravity — examples get written against it, new users conflate it with NMP, and its convenience quietly competes with the two-noun surface for attention. R3 (rederivability) and the docs rule (raw surface first, component second) are mitigations, not cures. If at some point the kit's needs start driving SDK changes that no plain app asked for, that is the §4 tripwire ("a second mechanism appearing") wearing UI clothes — stop and re-read this note.

**Owner decisions surfaced:**

1. **Fund the L3 business at all?** L1+L2 deliver most of the correctness value at a fraction of the maintenance. L3 is the visible product ask — but it's the expensive, risky layer. If yes: which platform first, and accept a hard budget of two platforms.
2. **Sibling repos vs `Packages/` in-repo for L2/L3?** Recommend sibling repos: it makes R1/R4 physically true (components *cannot* see internals) rather than review-enforced, at the cost of release-coordination friction.
3. **Gate L2/L3 on the M5 verdict?** Recommended yes (§6); confirming makes it policy.
4. **Distribution: linked packages with slots (recommended, per the three-fork evidence) vs shadcn-style copy-in.** If the owner has a strong copy-in preference, the compromise is: copy-in only for leaf-simple stateless components, linked for anything composite — but no copy/lock tooling either way.
