# NMP public-interface design guidelines & builder-manual TOC

- **Date:** 2026-07-11
- **Status:** foundational design pass for the builder manual. Provisional-until-v2 like everything else, but this is the north-star document other manual chapters must align with. Where this document and the truth anchors disagree, [`README.md`](../../README.md) (build state) and [`docs/VISION.md`](../VISION.md) (design) win.
- **Audience:** authors of the builder manual, and anyone extending NMP's public surface.

---

# Part A — Design guidelines: how to think about NMP's public interfaces

## A.0 The one-paragraph frame

NMP is an embeddable Nostr sync-and-routing engine — a library an app talks to, not a framework an app lives inside. The public surface is deliberately tiny: **two nouns** (a live query, a write intent), **one input** (the active identity), a small set of **capabilities** the app plugs in, and a **diagnostic surface** that renders what the engine did. Everything the engine uses to make a decision — demand, routing, ordering, row identity, admission — crosses the boundary as a **closed, introspectable value**, never as app code. Everything downstream of delivery is the app's, and the app may use arbitrary code there. That one sentence — **values in, code after** — is the governing rule for every interface decision in this document, and every guideline below is a corollary of it.

## A.1 The primitives

These are the load-bearing public primitives. There should never be more of them than fit in this table. When a new need appears, the first question is always "which existing primitive does this extend?" — a genuinely new primitive is a Tier-A event (adversarial propose/refute, per VISION §6), not a PR.

| Primitive | Status | What it IS | What it OWNS (engine) | What the APP owns |
|---|---|---|---|---|
| **Live query** (the read noun) | BUILT | A Nostr `Filter` whose field values are `Binding`s (`Literal \| Reactive(identity) \| Derived(inner Filter → closed Selector) \| SetOp`), handed to `observe`. A plain, hashable, serializable *value*. | Binding resolution and incremental re-eval; wire routing (outbox, lanes, 2-relay-min, capped fan-out, coalescing); sync (negentropy-first, coverage watermarks); delivery of rows + a typed coverage state; demand refcounting keyed on the descriptor; teardown on handle drop. | Which queries exist and when; folding delivered rows into its own view state with arbitrary code; observation scope (task lifetime / collection scope). |
| **Write intent** (the write noun) | BUILT | A durable, acknowledged operation: unsigned template + durability class (`durable \| ephemeral \| at-most-once`) + routing class (`authorOutbox \| toInboxes \| privateNarrow`). Returns a receipt whose status *streams*. | Signing orchestration (via the signer capability), route resolution through the same lane machinery reads use, per-relay send and ack, durability semantics, narrow-only private routing (fail-closed). | Composing what to write and when; presenting in-flight status. The app never picks relays and never confuses enqueued with converged — the types don't let it. |
| **Identity** (the one input) | BUILT | `addAccount` + `setActiveAccount(pubkey?)`. The app states a fact; there is no session model, login flow, or account vector in the engine. | Everything *derived* from the input: re-rooting the entire binding graph (teardown-before-activate, reverse-of-open), the account's relay lists, discovered outboxes, follow expansion, which key signs. | Who is active, and every scrap of login/session/onboarding UX. |
| **Capabilities** (signer; encrypt/decrypt; AUTH policy) | PARTIAL (local nsec signer BUILT; decrypt feedback path and NIP-42 AUTH policy PLANNED) | Pluggable engine-side abilities the app selects or injects. A capability is something the engine *invokes at the right moment*; the app configures which one exists, not when it runs. | Invocation timing and placement (sign at the awaiting-capability stage; decrypt where the key lives, before emitting raw tokens; AUTH challenge handling at the transport edge under an app-injected *policy value*). | Choosing/supplying the capability (local key, NIP-46, platform signer) and the policy values (e.g. "which relays may I AUTH to as whom"). Policies are declarative values, not callbacks — see A.3. |
| **Diagnostics** | BUILT (engine + FFI surface; falsifier screen in progress) | A live, read-only projection of the other planes: per relay — wire-sub count, lane breakdown, the *exact* filters sent, events received per kind, per-filter coverage state. | Producing every number from real engine state (never estimated), keeping it off the data path (observing diagnostics can never influence routing or delivery). | Rendering it. Apps are encouraged to ship a diagnostics screen permanently — it is the acceptance test made visible. |
| **Collection observation mode** | PLANNED (Tier-A at its gate; VISION §10) | An opt-in *observation mode* of the live query — NOT a third noun. Adds engine-maintained ordering, a bounded window, pagination, and merge of a *list* of queries, over the same demand node a plain `observe` would use. | Stable row ids and deterministic order via closed `OrderKey`/`RowKey` vocabularies; widen-only `loadMore` (grows the node's own since/until/limit); `has_more`/`exhausted`/`gap` as coverage surfacing; two-cursor separation (source cursor ≠ presentation cursor, candidate ledger #13). | Virtualization (which visible rows to instantiate) and all rendering. NMP owns the virtualizable collection, not the virtualization. |
| **Delivery-side transforms** (WoT filter; app sort) | PLANNED | Post-facto closures applied to *delivered* rows, on the app side of the boundary — the sanctioned home for app code in the read path. | Nothing beyond delivering rows the transforms can consume. If a transform's *inputs* are Nostr state (e.g. a WoT set derived from follows-of-follows), that input is itself a live query/`Derived` binding the engine maintains. | The transform code itself. A WoT scorer, a custom comparator, a mute heuristic — arbitrary app logic, applied after delivery, never parameterizing demand, routing, or engine cursors. |

Two structural notes the manual must repeat until they are boring:

1. **Nothing here is a third noun.** Identity is an input; capabilities are plug points; diagnostics are a projection; Collection is a mode of the read noun; transforms live after delivery. The recurring failure mode this table guards against is a "resource"/"session"/"module" concept creeping in beside the two nouns. Any proposal that reads like one is the old fragmentation returning (VISION §4, tripwire).
2. **Every closed vocabulary is extend-don't-escape.** `Selector`, `OrderKey`, `RowKey`, durability classes, routing classes, lanes, identity fields: a use case outside the vocabulary extends the vocabulary (a design event with a gate), it never admits a closure. This is the Electric-SQL/Replicache lesson: introspectable values are what let the engine hash, dedup, coalesce, and route demand; one opaque closure anywhere in the decision path and the whole demand-routing story collapses.

## A.2 The batteries-included boundary — the central judgment

The question: NMP could ship canned helpers — a "profile" query builder, a "thread" filter, a "reaction" write, a feed/Collection helper, a Blossom upload client. How far do we go before we've rebuilt the v1 framework with friendlier syntax?

### The principle

> **NMP ships the mechanism. A convenience is blessed only when it is a pure, introspectable composition of the existing primitives that encodes a *protocol fact* rather than a *product decision*, that a second unrelated app would write byte-for-byte identically, and that never becomes the only door to what it wraps.**

Operationally, a proposed battery must pass **all five** of these tests:

1. **Desugars to public values.** The helper must be expressible as a pure function returning an `NMPFilter` / `WriteIntent` / observation descriptor built entirely from the public surface. No private engine access, no privileged code path. The litmus: *could this exact helper live in a third-party package with zero special access?* If yes, it may ship in NMP's convenience layer. If no, it is either engine mechanism (→ Tier-A surface change) or app policy (→ rejected).
2. **Protocol fact, not product decision.** Kind numbers, tag names, NIP-10 marker conventions, NIP-51 list shapes, the filter shape for "reactions to event X" — protocol facts; encoding them once is exactly what a protocol library is for. "What counts as a conversation," "which replies rank first," "what a home feed contains," "how a mention renders" — product decisions; each app must answer differently, so a blessed answer is a leak. v1's `nmp-note-feed` died on precisely this line (feed *mechanics* were legitimate; baked *render policy* was one app's surface pretending to be framework).
3. **The second-unrelated-app test.** Would two apps with different products (a podcast player and a group-chat client, say) write this helper *identically*, down to the parameter list? If the parameters would differ, the helper is under-general; if the body would differ, it's product policy. Only identical-in-both passes. (This is v1's D0 generic-layer-purity test, kept because it was one of the few principles that actually predicted rot.)
4. **Never the only door.** The primitive a helper desugars to stays public, documented first, and the helper's expansion is printable — a builder can always ask "what filter did `.follows(of:)` actually build?" and get the value. Conveniences are shortcuts through the manual, not gates in front of it. The moment a capability is reachable *only* through its helper, the helper has become surface and the grammar has silently grown a closed-source production.
5. **Deletable.** Removing the helper must break no engine invariant and no ledger entry — worst case, callers inline the value it returned. This caps the compat cost of the whole layer: the grammar is forever; a helper is a deprecable function.

### What this admits and what it refuses

**Blessed (ship as an `NMPRecipes`-style convenience module — thin, value-returning, separately deletable):**

- `Filter` recipes for protocol shapes: `.profile(pubkey)` (kind:0), `.follows(of: binding)` (the kind:3→Tag(p) `Derived`), `.myFollowsNotes(kinds:)`, `.reactions(to: eventId)`, `.thread(root: eventId)` *as the NIP-10 filter shape only* — membership, not ordering or nesting policy.
- `WriteIntent` recipes: `.textNote(content:)`, `.reaction(to:content:)`, `.contactList(...)` — templates that fill kind/tags per NIP, leaving durability/routing visible.
- Named compound bindings: `followsMinusMutes(...)` as sugar over `SetOp(Diff, …)`.

**Engine mechanism, not sugar (belongs behind the surface or in a closed vocabulary — a helper can't carry correctness):**

- Anything involving ordering, windowing, cursors, row identity → the Collection mode's closed vocabularies. Cursor correctness cannot ride on app-side or helper-side code (the two-cursor bug class, candidate ledger #13).
- **Safe replaceable read-modify-write** (follow/unfollow, list edits). This is the hardest case and the manual must treat it as such: the classic "client wiped my follow list" bug is a *write built from stale state*. A naive `.follow(pubkey)` helper would re-create that bug class in blessed packaging. The right shape is a mechanism-level answer — an edit that can state its base (the current winner's id + a proven coverage watermark) and fail typed when the base is stale or coverage is `Unknown` — likely a future ledger entry. Until that exists, NMP ships the *filter and template* recipes but not a one-call `follow()`; the manual documents the safe pattern explicitly instead.
- WoT as a delivery-side filter *application point*: the point where the closure runs is engine-adjacent SDK mechanism; the scoring is app code.

**Engine-adjacent opt-in modules (more than sugar, less than core — allowed, but outside the two-noun surface and gated by the same five tests where applicable):**

- **Blossom media.** Upload/fetch needs an HTTP client, hash verification, server-list fallback, and signed auth events — real correctness machinery, so it exceeds "pure composition," but it is not a third noun either. It ships as a separate opt-in package that consumes the engine's signer capability through a public seam and the two nouns for server-list discovery (kind:10063 is just a query). It must never grow its own relay opinions or become a required import.
- The north-star UI component packages: strictly consumers of the two nouns, per platform, opt-in, dogfood value included.

**Refused, permanently:**

- Any helper taking an app closure that parameterizes engine behavior (comparators, lane-mappers, admission predicates). This exact shape — "protocol modules register closure lane-mappings" — was formally rejected at the Collection gate as the v1 feed framework reborn.
- Blessed display/formatting of any kind (ledger #12: the vocabulary to express presentation is absent from the engine, and the convenience layer does not get to re-add it).
- A "default app shell," blessed state container, or any scaffolding an app must adopt. The falsifier kill condition (M5) is the permanent guard: if using a battery makes the app NMP-shaped, the battery is wrong.
- Relay-picking conveniences of any form. There is no `relays:` parameter to sugar over (ledger #3), and no helper may reintroduce one.

### Why the line sits here (the defense)

- **v1's autopsy is the argument.** A wide, batteries-heavy surface required 46 principles, doctrine-lint, and recurring audits to police, and still rotted (display-separation violated ×27; the feed framework baked one app's render policy). "Does any app use it" proved to be the wrong admission question; "would a *second unrelated* app write it *identically*" is the one that predicted rot. Batteries that are pure value compositions cannot rot this way because they have no seam to leak through — they *are* the values.
- **Misuse-resistance is a shape property.** The bug-class ledger works because wrong programs don't compile or can't reach the wire. Every battery admitted under the five tests preserves that: it constructs the same values a hand-written call would, so every ledger mechanism still applies to it. A battery admitted *outside* the tests (a closure, a policy, a privileged path) would be a new seam the ledger doesn't cover — which is how governance-by-policing gets re-invented.
- **The cost model stays sane.** Grammar and noun changes are Tier-A events with adversarial gates because they are forever. Helpers under test 5 are deprecable functions. Keeping the boundary crisp keeps the expensive category small.
- **It still gets to feel batteries-included.** The manual's promise — "your first app in 20 lines" — is delivered by recipes over a small grammar, not by a big surface. TanStack Query won the same way: a tiny core contract, an ecosystem of thin value-level conveniences, and nobody builds "a TanStack Query app."

## A.3 The cross-platform contract

The same primitives manifest as Swift `AsyncSequence`, Kotlin `Flow`, TS async iterators, Rust `Stream`/channels, and a TUI. The contract for keeping one product across five faces:

1. **The nouns are the invariant; the delivery is the dialect.** `Filter`/`Binding`/`Selector`, `WriteIntent`/durability/routing, `Row`/`Coverage`, receipt states, diagnostics rows: identical shapes and names (modulo casing) on every platform, because they are serializable values defined once at the FFI seam. What varies per platform is *only* the reactive wrapper and the ownership idiom.
2. **Deliver via the platform's canonical cold reactive primitive** — Swift `AsyncSequence`, Kotlin cold `Flow` (caller applies `stateIn(WhileSubscribed)` themselves, the Room idiom verbatim), TS `AsyncIterator`, Rust `Stream`/blocking `recv`, TUI = the Rust handle rendered as text. Never invent an NMP observer/callback type as the primary API on any platform.
3. **Teardown rides the platform's natural ownership edge.** Swift: deinit/ARC (drop the iterator, demand drops). Kotlin: flow-collection scope. Rust: `Drop`. Explicit `cancel()` exists everywhere but is never *required*. The engine's teardown-with-grace debounce makes all of these safe.
4. **Detachable handle first; view-binding sugar second.** The `AsyncSequence`/`Flow` handle is the primary API; `@Observable` / `StateFlow` / signal adapters are thin optional layers on top (SwiftData's retrofit lesson — a view-only binding as the primary API is a trap that takes years to undo).
5. **Capabilities and policies, not callbacks.** Where the app must influence engine behavior (signer choice, AUTH policy, future knobs), it supplies a capability object or a declarative policy *value* at construction/registration time. The engine never calls back into app code to make a routing/demand/ordering decision mid-flight. (App closures over *delivered* data — A.1's transforms — are the sanctioned exception, because they sit after the boundary.)
6. **Ergonomic overloads may add convenience, never expressiveness.** A platform SDK may add builders, literals, default arguments; it may never add a closure-shaped or platform-only way to express something the value grammar can't. If Swift can say it and the FFI value can't carry it, the Swift API is wrong.
7. **Pay-as-you-go, no imposed lifecycle.** One construction call; every further feature is a method on that object. No provider/container/environment wrapper, no scene-phase hooks, no mandatory background task the app must schedule. Two calls for a small app; twenty for a full client; zero architecture either way. (This is M4's kill condition, kept as a permanent design rule.)
8. **Errors are typed states on handles and receipts**, not exceptions sprinkled through streams. Construction can throw; a running query's problems surface as coverage/diagnostic state; a write's problems surface as receipt states.
9. **Parity is tracked, not assumed.** A capability "exists" when it exists on every tier-1 platform or is explicitly marked platform-pending in the manual. The TUI/CLI (`nmp-demo`) is a real consumer, not a toy — it is the fastest place to see the nouns without a UI framework in the way, and manual examples should use it liberally.

## A.4 The philosophy — what a manual author must hold

1. **Correctness lives in the API's shape, not in a police force patrolling it.** The manual never says "don't do X" where the true answer is "X is unrepresentable." Prefer "there is no `relays:` parameter" over "avoid passing relays." Where a bug *is* representable (the replaceable read-modify-write), say so honestly and show the safe pattern — and treat that honesty as pressure to close the hole structurally.
2. **Two nouns, forever.** Every feature is presented as: a query you observe, a write you intend, or configuration of the machinery that serves those two. If a chapter can't be framed that way, the chapter — or the feature — is mis-designed.
3. **Identity is an input.** "The current signer is A… now B" is the whole contract; everything account-shaped downstream is derived by the engine. The manual should show account switching as one line and let the diagnostics screen prove the re-root.
4. **The diagnostic surface is the acceptance test made visible.** Debugging NMP is *reading*, not printf: what was asked, per relay; what arrived, per kind; what coverage is proven. The manual teaches the diagnostics screen in chapter one's orbit, not as an appendix — it is how a builder learns to trust (and verify) invisible-by-design routing.
5. **Empty and unknown are different types.** Coverage watermarks are the difference between "there are no results" and "we can't know yet," and the API keeps them apart. Every read example in the manual should show what it does with `Coverage`.
6. **Raw tokens out; presentation is the app's.** Hex pubkeys, Unix timestamps, verbatim kind:0. The manual demonstrates formatting *in app code* so nobody mistakes its absence from the engine for an oversight.
7. **The manual is truth-anchored.** Every chapter states whether its subject is BUILT or PLANNED, and links the ledger/README rather than over-promising. NMP's docs never claim a mechanism holds before a test proves it; the manual inherits that discipline.

---

# Part B — Provisional TOC for the NMP builder manual

A manual, not an API reference: mental model + how-to-do-things, with worked cross-platform examples. Status marks: **BUILT** (running, verified), **PARTIAL**, **PLANNED**. ★ = chapters that most need worked cross-platform examples (Swift + Kotlin + Rust/TUI minimum; TS when it exists).

### Part I — The mental model

1. **Why NMP exists** — the machinery every correct Nostr client re-implements; library-not-framework; what "misuse-resistance by shape" means for you. *(BUILT — conceptual)*
2. **The two nouns** — live query + write intent; the ownership table (engine / app / UI framework); why there is no third noun and what to do when you think you need one. *(BUILT)*
3. **★ Your first app in 20 lines** — construct engine → add account → observe → render → publish; per platform, side by side; the pay-as-you-go promise demonstrated. *(BUILT for Swift + Rust/CLI; Kotlin PLANNED (M6); TS unconfirmed)*
4. **How the engine works (optional depth)** — the four planes (store / resolver+router / outbox / diagnostics); what "replace-not-rebuild" and "recompile-not-reopen" buy you; read this to build trust, skip it to build apps. *(BUILT)*

### Part II — Reading

5. **★ Live queries and the binding grammar** — `Literal`/`Reactive`/`Derived`/`SetOp`; worked: `$myFollows` notes, NIP-29 groups-I'm-in (depth 2), follows-minus-mutes; why `Selector` is closed and what to do at the vocabulary's edge. *(BUILT — the crown jewel chapter)*
6. **★ Consuming results: rows, snapshots, coverage** — RowBatch semantics; folding streams into your own state; `@Observable`/`stateIn` sugar; empty vs unknown, and what your UI should do with each. *(BUILT)*
7. **Feeds and the Collection observation mode** — ordering, bounded windows, `loadMore`, `has_more`/`gap`, merging query lists; NMP owns the virtualizable collection, your framework owns virtualization. *(PLANNED — Tier-A pending; write as design-preview until ratified)* ★ when built
8. **Delivery-side transforms** — the sanctioned home for app code in the read path: WoT post-facto filtering, custom sort; why these never touch demand or cursors. *(PLANNED)*

### Part III — Writing

9. **★ Writing: intents, receipts, durability** — the receipt state stream (accepted → signed → routed → per-relay acked); durable vs ephemeral vs at-most-once; routing classes incl. fail-closed private narrow; presenting in-flight status. *(BUILT; pre-signed publish across FFI PARTIAL)*
10. **Editing replaceable state safely** — profiles, contact lists, NIP-51 lists; the read-modify-write trap ("the client that wiped my follows") and the coverage-aware safe pattern; where the structural fix is headed. *(PARTIAL — pattern documentable today, mechanism PLANNED)*

### Part IV — Identity & capabilities

11. **★ Identity and multi-account** — identity as input; add/switch/logout-to-read-only in three lines; what re-rooting means; verifying zero cross-account leakage on the diagnostics screen. *(BUILT)*
12. **Capabilities: signer, encrypt/decrypt, AUTH policy** — capabilities vs callbacks; local nsec today, NIP-46/platform signers later; engine-side decryption and why the key lives with the engine; app-injected AUTH policy values. *(PARTIAL — local signer BUILT; decrypt path, NIP-46, NIP-42 PLANNED)*

### Part V — The network (you never pick relays)

13. **Relays: outbox, indexers, lanes** — self-bootstrapping outbox from two indexer relays; lanes and roles; why there is no `relays:` parameter anywhere; operator config as policy, not routing override. *(BUILT — live-proven)*
14. **Offline and sync** — cache-serve-first; negentropy-first against probed relays; coverage watermarks and authoritative cache misses; reconnection replay; cold-start offline as a feature. *(BUILT)*
15. **★ Diagnostics and debugging** — reading the surface: per-relay subs, exact filters, events per kind, per-filter coverage; answering "why is my feed empty?" and "why did we route there?"; shipping a diagnostics screen permanently. *(BUILT — FFI surface landed; falsifier screen in progress)*

### Part VI — Extended surfaces

16. **Media (Blossom)** — the opt-in module shape: signer-capability consumer + two-noun discovery; hash verification and server fallback. *(PLANNED)*
17. **Search (NIP-50)** — the reserved `search` filter field; search-relay lane. *(PLANNED)*

### Part VII — Platform guides

18. **iOS / Swift** *(BUILT)* · **Android / Kotlin** *(PLANNED — M6)* · **Rust** *(BUILT — `Handle`)* · **TypeScript / web** *(PLANNED — unconfirmed for v2)* · **TUI / CLI** *(BUILT — `nmp-demo` as reference consumer)* — one short guide each: idiomatic delivery, ownership/teardown idiom, sugar layers, platform gotchas. Structured so a reader reads exactly one.

### Part VIII — Judgment

19. **Patterns and anti-patterns** — the bug-class ledger retold from the builder's seat: twelve bugs you *can't* write and what the compiler/typed error says when you try; the few you still can, and their safe patterns. *(BUILT — ledger-derived)*
20. **What NMP does not do (and why)** — the scope fence: no app architecture, no display policy, no relay picking, no wallet/DMs/MLS in v2; the batteries boundary (A.2) as reader-facing promise: what helpers exist, what will never exist, and how to tell which side a request falls on. *(BUILT — conceptual)*

**Appendices:** A. Grammar reference (the closed vocabularies, verbatim) · B. The bug-class ledger (linked, not duplicated) · C. Glossary (lane, watermark, coverage, atom, re-root, demand) · D. Manual status table (BUILT/PLANNED per chapter — the manual's own truth anchor).

### Notes for the chapter authors

- Chapters 3, 5, 6, 9, 11, 15 carry the manual: they need real, runnable, cross-platform worked examples (Swift + Rust/CLI now; Kotlin added at M6). Everything else can lean on prose + one platform.
- Chapters 7, 8, 12, 16, 17 must be written honestly as PLANNED (design-preview framing, linked to VISION/gates) — the manual inherits the truth-anchor discipline (A.4.7).
- Chapter 19 is the cheapest high-trust chapter: the ledger already contains the material; retell it as developer experience ("here's the compile error you get"), not as governance.
