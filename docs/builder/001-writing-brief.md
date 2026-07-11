# NMP Builder Manual — governed writing brief

This brief governs the North Star builder manual. Read
`000-design-guidelines-and-toc.md` first: it holds the current ownership
boundary and provisional target contract. This brief is not a frozen API spec;
when an architecture decision changes, update both files and every affected
chapter in the same reviewed change.

## What this manual IS (and is not)
- A **North Star builder guide**: how NMP works, the mental model, and **how to DO things** with worked, runnable examples across iOS/Android/Rust/TS/TUI. It is directional agreement on where the public contracts are headed — NOT a hard implementation constraint, and NOT an API reference.
- The reader spans first-time-Nostr devs and experienced ones migrating from NDK/Applesauce.

## Style guide (every chapter follows this)
1. **Voice:** direct, concrete, second person ("you"). Lead each chapter with a one-sentence "what you'll be able to do after this." No marketing fluff.
2. **The two nouns are the spine:** a **live query** and a **write intent**.
   Everything traces back to them. For the target contract, a query's semantic
   identity is `Selection + SourceAuthority + AccessContext`; `Binding` remains
   a closed value grammar; `$currentPubkey` is a reactive/default identity
   input; snapshots carry rows plus compact cache, acquisition, and shortfall
   evidence; receipts expose persisted per-relay facts; diagnostics is the
   acceptance test made visible. When showing today's narrower `Filter`,
   `Coverage`, or signer API, label it as the current surface rather than the
   final meaning.
3. **BUILT vs PLANNED — never mislead.** Every chapter carries a top banner: `**Status: BUILT**` (works today, examples must be real/runnable) · `**Status: PARTIAL**` (some works — say exactly what) · `**Status: PLANNED**` (design preview — say "this is the intended shape, not yet shipped"). PLANNED chapters show the *intended* API, clearly marked.
4. **Examples:** for BUILT chapters, use the real current API. iOS examples use
   the actual Swift SDK (`NMPEngine`, `NMPFilter`, `.observe -> AsyncSequence`,
   `.publish -> Receipt`, `.setActiveAccount`, `.observeDiagnostics`, raw-token
   `Row`, and the current `Coverage`). Rust examples use the current facade.
   Kotlin JVM examples may use the built `Flow` package; Android/AAR/Compose,
   TS, and TUI examples show intended idioms only and must say so. Never
   present a current spelling as the settled target merely because it exists.
5. **Presentation is the app's job.** The engine emits raw tokens (hex pubkeys, Unix timestamps, verbatim kind:0). Examples format in app code, never expect the engine to.
6. **No app-expanded relay routing.** Never show a raw query or generic publish
   call taking an app-computed relay list. Routing is the engine's. Typed
   protocol context may carry protocol authority such as a NIP-29 group host
   relay; that is an inspectable module contribution, not a generic route
   escape hatch.
7. **Cross-reference** by chapter title in prose (e.g. "see *Coverage: empty vs unknown*"). Keep each chapter self-contained but linked.
8. **Length:** aim 800–2000 words per chapter; depth over padding. Code blocks are the point — favor them.
9. **File:** write to `docs/builder/NN-slug.md` with an H1 title, the status banner, then the body. Markdown only.

## The boundary principle
A reusable declaration or protocol operation belongs in NMP only when it
encodes protocol fact rather than product policy. Lightweight query fragments
must expand to public, printable values. Richer protocol operations may use a
bounded typed capability, but must still be deterministic, inspectable, and
unavailable when their opt-in module is absent. The core never blesses a
timeline, content kind, ranking, or presentation policy. Governing rule:
**values in, code after** — closed values for anything the engine routes, keys,
orders, signs, or persists; app closures only over delivered values.

## The modularity principle
The boundary principle governs the API surface; the modularity principle
governs protocol ownership and code weight. Non-core protocol functionality is
opt-in. A module owns only the exact event schemas and semantics defined by its
NIP, plus typed builders, parsers, protocol state, queries, operations, and
contextual routing facts for those schemas. It does not own a broad content
category or every foreign event used inside its context.

- Core remains content-agnostic: canonical events, demand, store, routing,
  sync, signing orchestration, receipts, diagnostics, and bounded capability
  seams.
- Protocol construction uses immutable unsigned drafts. Modules contribute
  only fields and context they own; core validates and signs the final body
  once.
- NIP-29 may add its required `h` tag and group-host routing context to a photo
  draft without owning the photo schema.
- The exact Cargo/SwiftPM/Kotlin packaging mechanism is provisional. It may not
  create a second engine facade or require an app container/registration
  lifecycle.
- Examples across the manual must be kind-diverse. A kind:1 or social-feed
  example is one probe, never the product's default model.

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
- `09 binding-grammar — Live queries & the binding grammar (literal, reactive, derived, and set-op examples) · BUILT · ★`
- `10 consuming-results — Consuming results: rows, snapshots, and presentation ownership · BUILT · ★`
- `11 coverage — Coverage: empty vs unknown (the trust chapter) · BUILT · ★`
- `12 collection-mode — Feeds & the Collection observation mode · PLANNED`
- `13 delivery-transforms — Delivery-side transforms: WoT filtering & custom sort · PLANNED`

### Part IV — Writing
- `14 writing — Writing: intents, receipts, and the durability guarantee lattice · BUILT · ★`
- `15 editing-replaceable — Editing replaceable state safely: the wiped-follow-list trap · PARTIAL`

### Part V — The hard concerns
- `16 identity — Identity & multi-account: re-root in one line · BUILT · ★`
- `17 relays — Compiled routes and typed protocol context · PARTIAL`
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
- `27 recipes-and-choosing — Reusable declarations, typed protocol operations, and app policy · PARTIAL`
- `28 patterns — Patterns & anti-patterns: the guarantees you build on (the bug-class ledger as DX) · BUILT`
- `29 not-do — What NMP does NOT do (and why) · BUILT`
- `30 platform-guides — Platform SDK guides: iOS, Android, Rust, TS, TUI · BUILT · ★`
- `31 gallery — Example gallery + graduating from the falsifier app · BUILT`
- `32 extending — Extending NMP: exact protocol ownership and immutable composition · PLANNED`
- `33 versioning — Versioning & stability: what "provisional-until-v2" means for you · BUILT`

## Notes for writers
- The falsifier app (`apps/Falsifier`), `nmp-demo`, and the live tests are your source of real, runnable examples — mine them.
- Swift, direct Rust, and the minimal JVM Kotlin/Flow package are built. Full Android/AAR/Compose, TS, and TUI remain unproved or uncommitted.
- When you reference a guarantee, name the bug-ledger entry it corresponds to (`docs/bug-class-ledger.md`).
- If your chapter's subject exposes a gap in the current build, note it honestly in an aside — do not paper over it.
