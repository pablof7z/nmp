# UI components across platforms — strategy design note

- **Date:** 2026-07-11
- **Status:** Historical ideation note, **roadmap recommendation superseded** by
  the v2 contract promotion. Its old-repo failure analysis remains useful. NMP
  currently blesses no content kind, feed recipe, component family, or first UI
  kit. Optional protocol/feature modules may expose exact-schema codecs,
  builders, typed queries, and contextual operations; rendering remains app/UI-
  framework territory.
- **Anchors:** VISION P1 (library, not framework), §3 ownership table (rendering/layout/navigation belong to the UI framework), §10 ("values in, code after"; Collection observation mode), bug-ledger #11 (no app-owned expansion) and #12 (no presentation in core); README "Who owns what."
- **Cautionary ground truth (old repo, `nostr-multi-platform`):** `crates/nmp-component-registry/` (122 files, 5 platform targets), `crates/nmp-content/src/{tokenizer,grouper}.rs`, `docs/recipes/content-rendering.md`, `docs/recipes/app-shapes.md`, `docs/cli.md` §"add component"; the three independent consumer-app forks of `NostrInlineVideoPlayer.swift` (evidence in §2.3).

---

## 1. The problem, honestly stated

This exploration began from a hypothetical NMP-owned component program intended
to save apps from rebuilding common social UI. The promoted contract does not
adopt that program: choosing those components would privilege one content model
and pull NMP back toward an application framework. The old repo still provides
valuable evidence about how "just helpers" re-annexed the app, so that analysis
is retained below without treating its examples as a roadmap.

The four constraints this design must hold simultaneously:

1. **Library, not framework (P1).** The README assigns rendering, layout, navigation to the UI framework. A component story must not silently re-annex them.
2. **Two nouns + "values in, code after."** Anything the engine uses to *decide* is a closed introspectable value; app closures may fold *delivered* rows into view state but never parameterize engine behavior. Rendering any delivered protocol data must land cleanly on one side of that line.
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
| **L1 — protocol-semantic data** | Pure functions from delivered rows to typed values with closed vocabularies. Each opt-in module owns only its exact NIP/feature semantics; no generic social-content taxonomy is privileged. No I/O, no engine parameterization, off the demand path. | **Genuinely cross-platform** — one Rust implementation, values across FFI | Legal and squarely inside "values in, code after": these fold delivered rows or build immutable drafts; the engine never consults presentation structure. Vocabulary names protocol facts, never card/row/column/gallery nodes. |
| **L2 — headless behavior (per platform)** | A possible future thin observable adapter around an opt-in protocol/feature module's public values and the ordinary query/write APIs. | Per-platform idiom (`@Observable` / `ViewModel`+`Flow`). | Not on the current roadmap. Legal only if public-surface-only and if it does not establish an NMP-prescribed app architecture. |
| **L3 — reference views (per platform)** | Possible future pixels consuming public SDK/module values. | **Not shareable.** N implementations, one per platform. | Not on the current roadmap. If revisited, they must remain ordinary optional consumers, never the default path or a kind-dispatch system. |
| **App** | Screen composition, navigation, theming decisions, resource-lifecycle policy (media playback, prefetch), anything it replaces. | — | — |

The pivotal distinction, inherited from the old repo's late realization (its #3113: *codec vs formatting*): **codec** — mapping between protocol representations (hex↔npub, content→token tree, rows→reply tree) — is shared, reusable, one-implementation work. **Formatting** — truncation, locale dates, "2h ago", display-name fallback chains — is app-owned presentation and appears in *no* NMP layer, not even L3's reference views except as trivially replaceable view code. L1 is all codec; the engine stays all-raw; formatting never gets a shared home.

## 4. The design space

### Historical Option A — protocol-semantic values (not a selected first artifact)

The old tokenizer remains a possible harvest candidate, but it is not a blessed
`nmp-content` roadmap or the default semantic layer. The general legal shape is
an opt-in protocol/feature crate exposing closed typed values or immutable draft
builders through Rust and platform projections. A candidate earns a module by
owning exact protocol semantics and proving cross-platform value; popularity of
one content kind is not sufficient.

- **Shared:** everything — one Rust implementation is the whole point; identical NIP-21/27/30 semantics on every platform is a correctness property, not a convenience.
- **Constraints:** honors all four. Opt-in crate = zero weight (C3). Values with closed vocabularies (C2). No rendering opinion whatsoever (C1). Fully portable because it's data (C4).
- **Failure mode:** vocabulary creep — segments that describe *appearance* (gallery, card, collapsed-run) rather than protocol facts. The tripwire is nameable: any variant whose meaning you'd explain by describing pixels is a render-IR node. Second failure: an `EventRef` token tempts a "resolver" abstraction back into existence (the old `refs.event.envelopes` chain). v2 answer: an event ref resolves by *the app running another live query* — the engine already dedups and caches; there is no third resolution machinery to build.

### Historical Option B — Headless/behavior components (L2)

Per-platform packages of concept view-models. Honest inventory of what behavior actually remains once the engine exists: **less than TanStack's, by design.** Query lifecycle, caching, pagination, ordering, deltas, receipt state machines — all engine. What's left: draft/composition state, autocomplete orchestration, L1 folding, and platform-reactive packaging. Each headless component is thin — which is the *proof the engine's surface is right*, and the first place a gap would show (dogfooding, VISION §3's "second job").

- **Shared:** the shape and the L1 substrate; the code is per-platform idiom.
- **Constraints:** honors C1 iff zero-scaffold (drop into a bare view, no host/provider) and public-surface-only. C2: legal by construction — folds delivered rows, submits intents. C3: separate packages, trivially zero-weight. C4: honest — behavior semantics port, code doesn't.
- **Failure mode:** the soft framework. A family of `Nmp*Model` types becomes "the NMP way to build screens," and docs start assuming them. Mitigation is structural (§5 R3/R4) plus a docs rule: every L2 component's documentation *shows the raw two-noun equivalent first*, the component second.

### Historical Option C — Per-platform reference component kits (L3, opt-in, outside the engine contract)

SwiftUI package + Compose artifact of actual views consuming L1+L2. This was the
most framework-shaped part of the hypothetical component program, and the part
the old repo demonstrated is dangerous. It would be legal only with the §5
rules; it is not selected by the promoted direction.

- **Shared:** nothing at the pixel layer. What's shared is that both kits sit on the same L1 values and the same public SDK — so their *correctness* is shared even though their code isn't.
- **Constraints:** C1 is the knife-edge — held only by the structural rules below. C2: fine (render side). C3: separate packages. C4: honest — this option openly commits NMP to N parallel UI codebases (the old registry's 122 files across 5 targets is the price tag from last time; budget for 2 targets max, ever).
- **Failure modes,** each mapped to its §2 ancestor: components needing engine-interior seams (§2.1) → R1/R4; kind-dispatch registries (§2.2) → R5; baked resource policy (§2.3) → R6; policing app copies (§2.4) → distribution choice in §5.
- **Distribution:** linked packages (SwiftPM/Maven) as the default, *reversing* the old copy-in model. The three-fork AVPlayer evidence shows copy-in failed both directions: the canonical stayed buggy, the forks diverged. With linked+slots, a bug fix ships once, and the legitimate per-app divergence (eager vs lazy playback) lives in slots rather than forks. Vendoring the source remains trivially possible for an app that wants full ownership — but NMP builds no copy/lock/update machinery (`nmp add component`, `nmp.components.lock`, hash-diff updates were themselves framework tooling; never rebuild them).

### Option D — Cross-platform render-IR (rejected, and worth rejecting precisely)

A Rust-side declarative UI description (nodes like card/column/text-style) that platform interpreters render. Rejected on all four constraints at once: it *is* a UI framework (C1 — rendering and layout re-annexed wholesale); its node vocabulary is exactly the "visual structure" a closed value must not encode (C2 in spirit); it is mandatory weight for anything rendered through it (C3); and it lands on lowest-common-denominator or grows per-platform escape hatches, the two classic ends of every write-once-render-anywhere attempt (C4). The old repo's `ContentTreeWire` was only a *mild* IR — semantic tokens, no layout nodes — and even it pulled kind-registries, dispatchers, and hosts into existence around it. A real IR is that gravitational field squared. The one thing to keep from considering it: the discipline it clarifies — **the shareable cross-platform artifact is what content *is*, never what it *looks like*.**

## 5. Promoted direction

**No blessed content/component roadmap in v2.** NMP first proves the generic live
query and write-intent engine across kind-diverse applications. Reusable
semantics enter through opt-in protocol/feature modules that own exact schemas
or contribute typed context to immutable drafts. NMP does not select a note
card, thread view, social feed, content tokenizer, or platform kit as the next
canonical layer.

The six rules below remain useful **conditional gates if UI packages are ever
revisited**. They are not a commitment to build L2/L3.

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
| Rows, receipts, acquisition evidence, diagnostics — raw protocol values only | Exact-schema NIP/feature modules; immutable builders; typed protocol queries/context operations | Screen composition, navigation |
| Generic store, routing, signing, retry, and query correctness | Pure protocol-semantic helpers when a module justifies them | Formatting, content taxonomy, display-name policy |
| — | No official L2/L3 package currently planned | Resource policy, theming, rendering, and component selection |

## 6. Staging

1. **Now:** ship no component or generic content package. Use kind-diverse
   falsifiers to prove the core and provisional SDK surface.
2. **As protocols demand it:** add opt-in modules around exact NIP/feature
   semantics. Schema builders, typed queries, and contextual operations must
   remain introspectable and must not privilege unrelated content kinds.
3. **After diverse evidence:** consider pure semantic helpers only when at least
   two materially different applications demonstrate the same protocol-owned
   need. Popularity of kind:1/social-feed UI is not an architectural argument.
4. **Revisit UI kits later, explicitly:** there is no first platform, first card,
   or component distribution decision today. Any future proposal starts from
   R1–R6 and must survive a fresh thesis review.
5. **Never:** copy/lock/update component tooling; kind registries; component
   hosts; a render IR in core.

## 7. Honesty — what cannot be reconciled, and the owner decisions

**The original hypothetical ask, taken literally, was not fully satisfiable.**
"Offer UI components so builders don't re-implement per platform" implies shared
component *code*. There is no such thing across SwiftUI/Compose/web without
building the render-IR (Option D), which is the framework this rebuild exists to
escape. The old repo's 122-file, five-target registry records what that business
costs when unbudgeted; the promoted direction declines to start it.

**The residual thesis tension that never fully dissolves:** any official component kit, however governed, exerts gravity — examples get written against it, new users conflate it with NMP, and its convenience quietly competes with the two-noun surface for attention. R3 (rederivability) and the docs rule (raw surface first, component second) are mitigations, not cures. If at some point the kit's needs start driving SDK changes that no plain app asked for, that is the §4 tripwire ("a second mechanism appearing") wearing UI clothes — stop and re-read this note.

**Promotion decisions:**

1. Do not fund or sequence an official L2/L3 component business in the current
   v2 frame.
2. Do not choose sibling-repo/in-repo distribution, a first platform, or a first
   component before a later explicit review.
3. Let exact-schema protocol/feature modules prove reusable semantics without
   turning one content family into NMP's default product model.
4. Keep the old-repo evidence and R1–R6 as rejection tests for any future
   proposal, not as a latent implementation queue.
