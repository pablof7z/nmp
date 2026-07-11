# NMP Builder Manual — writing brief (final TOC + style guide)

This brief governs the North Star builder manual. It synthesizes Fable's design guidelines (`000-design-guidelines-and-toc.md`) with three Opus reviews (platform-consumer, kernel-correctness, adoption/DX). Each chapter is written by one agent. Read `000-design-guidelines-and-toc.md` first — it holds the boundary principle and the primitives.

## What this manual IS (and is not)
- A **North Star builder guide**: how NMP works, the mental model, and **how to DO things** with worked, runnable examples across iOS/Android/Rust/TS/TUI. It is directional agreement on where the public contracts are headed — NOT a hard implementation constraint, and NOT an API reference.
- The reader spans first-time-Nostr devs and experienced ones migrating from NDK/Applesauce.

## Style guide (every chapter follows this)
1. **Voice:** direct, concrete, second person ("you"). Lead each chapter with a one-sentence "what you'll be able to do after this." No marketing fluff.
2. **The two nouns are the spine:** a **live query** (a Nostr `Filter` whose field values are `Binding`s) and a **write intent**. Everything traces back to them. Use the real vocabulary: `Binding = Literal | Reactive(ActivePubkey) | Derived(inner Filter, closed Selector) | SetOp`; identity-is-input; coverage is `Unknown | CompleteUpTo(watermark)`; receipts stream a `WriteStatus`; the diagnostic surface is the acceptance test made visible.
3. **BUILT vs PLANNED — never mislead.** Every chapter carries a top banner: `**Status: BUILT**` (works today, examples must be real/runnable) · `**Status: PARTIAL**` (some works — say exactly what) · `**Status: PLANNED**` (design preview — say "this is the intended shape, not yet shipped"). PLANNED chapters show the *intended* API, clearly marked.
4. **Examples:** for BUILT chapters, use the REAL current API. iOS examples use the actual Swift SDK (`NMPEngine`, `NMPFilter`, `.observe → AsyncSequence`, `NMPQuerySnapshot`, `.publish → Receipt`, `.setActiveAccount`, `.observeDiagnostics`, raw-token `Row`, `Coverage`). Rust uses the `Handle`. Android/TS/TUI examples show the intended idiom (Flow / async iterator / render loop) — mark them PLANNED-shape if the SDK isn't built for that platform yet (only Swift + Rust are built today). Every ★ chapter needs at least the Swift + Rust example; show Kotlin/TS where the idiom differs meaningfully.
5. **Presentation is the app's job.** The engine emits raw tokens (hex pubkeys, Unix timestamps, verbatim kind:0). Examples format in app code, never expect the engine to.
6. **No `relays:` anywhere.** Never show an app picking relays. Routing is the engine's; the app configures indexers + policy only.
7. **Cross-reference** by chapter title in prose (e.g. "see *Coverage: empty vs unknown*"). Keep each chapter self-contained but linked.
8. **Length:** aim 800–2000 words per chapter; depth over padding. Code blocks are the point — favor them.
9. **File:** write to `docs/builder/NN-slug.md` with an H1 title, the status banner, then the body. Markdown only.

## The boundary principle (from Fable — the manual must teach it)
A convenience/"battery" is blessed only when it: (1) desugars entirely to public values (could live in a 3rd-party package with zero privileged access); (2) encodes a **protocol fact** (kinds/tags/NIP shapes), never a **product decision** (feed contents, ordering, display); (3) a second unrelated app would write it byte-for-byte identically; (4) leaves the wrapped primitive public and prints its expansion; (5) is deletable without breaking any bug-ledger guarantee. Governing rule everywhere: **values in, code after** (closed vocabularies for anything the engine routes/keys/orders; app closures only over *delivered* rows).

## The modularity principle (owner, load-bearing — the manual & the architecture must honor it)
The boundary principle governs the API *surface*; the modularity principle governs *code weight*. **Non-primitive protocol functionality is opt-in and modular — a per-NIP crate or a feature flag — so an app carries only what it uses.** The engine CORE is the two nouns + the hard concerns (store, routing/outbox, sync/negentropy, coverage, identity, diagnostics, the capability seams). Everything protocol-specific and non-primitive — reactions (`.react()`/`.unreact()`), reposts, follow packs, highlights, long-form, lists, comments, etc. — lives in its own opt-in module. A minimal app that never reacts must link **zero** reaction code; adding FollowPacks support must not tax every other app.

- This is the old NMP's genuine win: the reactions NIP crate encoded what reactions *mean*; apps that didn't care didn't pack `.react()`. Keep that.
- Mechanism (to be finalized in implementation): a per-NIP crate, or a Cargo `feature` flag on a protocol crate, or a registerable module — the manual teaches the *principle* (you pay only for the NIPs you enable) and shows the *shape* (enable a feature/module → its recipes + kinds appear; don't → they're absent), marked PLANNED where the mechanism isn't built yet.
- **This reframes:** *two-nouns* (core is tiny; protocol meaning is modular), *recipes* (each recipe/battery ships in its NIP module, not core; enabling the module is how you get `.reactions(to:)`), *extending* (adding a protocol module = the primary extension path, under the five boundary tests + this modularity rule), *packaging* (per-platform, you compose only the modules you enable — this affects binary size). Every writer touching these chapters must reflect it. It is also a durable design decision to record in the design guidelines / VISION, not just manual prose.

## Final TOC (each line: `NN slug — Title · status · ★=needs cross-platform worked examples`)

### Part I — Orient
- `01 why-nmp — Why NMP exists: the problem, library-not-framework, and why-not-NDK/Applesauce/roll-your-own · BUILT`
- `02 mental-model — The mental model in one diagram: values in → engine → rows + coverage out → your code after · BUILT`
- `03 status-map — What works today vs. what's coming (a BUILT/PLANNED map) + glossary of Nostr & NMP terms · BUILT`

### Part II — Get running
- `04 ten-minute-timeline — Build a working timeline in 10 minutes (keystroke tutorial, iOS) · BUILT · ★`
- `05 two-nouns — The two nouns and the ownership table · BUILT`
- `06 first-app — Your first app in 20 lines (per-platform, the shape) · BUILT · ★`
- `07 brownfield — Adding NMP to an app you already own · BUILT · ★`
- `08 packaging — Packaging, build & distribution (xcframework / cargo-ndk / wasm / SwiftPM) · BUILT`

### Part III — Reading (queries & results)
- `09 binding-grammar — Live queries & the binding grammar ($myFollows, groups-I'm-in, follows−mutes worked) · BUILT · ★`
- `10 consuming-results — Consuming results: rows, snapshots, and presentation ownership · BUILT · ★`
- `11 coverage — Coverage: empty vs unknown (the trust chapter) · BUILT · ★`
- `12 collection-mode — Feeds & the Collection observation mode · PLANNED`
- `13 delivery-transforms — Delivery-side transforms: WoT filtering & custom sort · PLANNED`

### Part IV — Writing
- `14 writing — Writing: intents, receipts, and the durability guarantee lattice · BUILT · ★`
- `15 editing-replaceable — Editing replaceable state safely: the wiped-follow-list trap · PARTIAL`

### Part V — The hard concerns
- `16 identity — Identity & multi-account: re-root in one line · BUILT · ★`
- `17 relays — Relays: outbox, indexers, and roles — you never pick relays · BUILT`
- `18 tracing-demand — "Where did my query go?" Tracing demand through the compiler · BUILT · ★`
- `19 offline-sync — Offline & sync: negentropy, coverage watermarks, and the limits of replay · BUILT`
- `20 capabilities — Capabilities: signer, AUTH policy, encrypt/decrypt · PARTIAL`
- `21 provenance — Provenance, and why private events can't be republished · BUILT`

### Part VI — Operate
- `22 diagnostics — Diagnostics & debugging: "why is my feed empty?" · BUILT · ★`
- `23 threading-lifecycle — Threading, the main-thread contract & app lifecycle (background/foreground) · BUILT`
- `24 performance — Cost & performance: the pay-as-you-go mental model · BUILT`
- `25 testing — Testing an app that embeds NMP · BUILT`
- `26 troubleshooting — Troubleshooting & FAQ (each answer read off the diagnostics) · BUILT`

### Part VII — Reference & judgment
- `27 recipes-and-choosing — The batteries: recipes catalog (each printing its desugaring) + choosing recipe vs compose vs own · PARTIAL`
- `28 patterns — Patterns & anti-patterns: the guarantees you build on (the bug-class ledger as DX) · BUILT`
- `29 not-do — What NMP does NOT do (and why) · BUILT`
- `30 platform-guides — Platform SDK guides: iOS, Android, Rust, TS, TUI · BUILT · ★`
- `31 gallery — Example gallery + graduating from the falsifier app · BUILT`
- `32 extending — Extending NMP: protocol modules & recipes under the five tests · PLANNED`
- `33 versioning — Versioning & stability: what "provisional-until-v2" means for you · BUILT`

## Notes for writers
- The falsifier app (`apps/Falsifier`), `nmp-demo`, and the live tests are your source of real, runnable examples — mine them.
- Only **Swift and Rust** SDKs are BUILT today. Kotlin/TS/TUI examples are intended-shape (mark PLANNED-shape).
- When you reference a guarantee, name the bug-ledger entry it corresponds to (`docs/bug-class-ledger.md`).
- If your chapter's subject exposes a gap in the current build, note it honestly in an aside — do not paper over it.
