# NMP v2 — Grand Vision & Validation Plan

- **Date:** 2026-07-11
- **Status:** Founding artifact for the new repo. Everything here is provisional-until-v2 per the process rules; nothing below is self-compat-binding before a v2.0 ships (not before Aug 2026).
- **Substrate:** `research/2026-07-10-nmp-first-principles-architecture-exploration.md` (authoritative design record). This document synthesizes; it does not re-litigate settled decisions.

---

## 1. The vision in one page

**NMP v2 is an embeddable Nostr sync-and-routing engine — a library an app talks to, not a framework an app lives inside.**

Today, every correct Nostr client re-implements the same brutal machinery: outbox routing, subscription lifecycle, replaceable-event semantics, dedup and provenance, cache authority, relay fan-out discipline. Every incorrect Nostr client skips some of it and ships bugs users can't see until their timeline is silently stale. NMP v1 attacked this by owning the whole application — actor, AppState, reducers, projections — and then policing the wide seam that created with a 46-principle corpus, doctrine lints, and recurring audits. The apps built on it don't work well, and a podcast player that touches Nostr for one feature had to buy an entire way of architecting itself.

v2 inverts the shape. The engine owns exactly the part of the problem that is Nostr's — a **local-first synchronizing replica** of the events the app cares about, plus all the **network correctness** required to keep it right — and nothing that is the app's. A SwiftUI developer who knows SwiftData or TanStack Query should be able to add NMP to a normal app in an afternoon: two calls for a small app, twenty for a full client, and the cost of adoption is proportional to use. There is no NMP app architecture to learn because there is no NMP app architecture.

The app-facing surface is **two nouns**:

1. **A live query** — a Nostr `Filter` whose field values are `Binding`s: literal sets, reactive identity references, or the projected output of *another* filter. Declaring `kinds:[1], authors := Derived(kinds:[3], authors:[$currentPubkey] → Tag(p))` is the entire program for "my follows' notes, forever correct": when the follow list changes, the engine surgically re-routes the wire subscriptions; when the active signer changes, the whole graph re-roots — with zero app code. The query arrives as the platform's native reactive primitive: Swift `AsyncSequence` + `@Observable`, Kotlin `Flow`.
2. **A write intent** — a durable, acknowledged operation. Handing an event to the engine returns a receipt whose status streams from pending through signed, routed, and per-relay acknowledged. Enqueued is never confused with converged; sign and publish are orthogonal.

Everything else — outbox routing that cannot be turned off, REQ coalescing per relay, 2-relay-minimum coverage with capped fan-out, negentropy-first sync against probed relays, coverage watermarks that make a cache miss authoritative, provenance that survives dedup — is engine interior, visible only through a **first-class diagnostic surface** that shows, per relay and per kind, exactly what was asked, what arrived, and what coverage has been proven. The diagnostics are not scaffolding; they are the acceptance test rendered on screen, permanently.

What makes v2 different is not any single mechanism — most exist in some form in v1. It is that **correctness lives in the API's shape instead of in its policing**. The old repo proved that a wide surface plus doctrine cannot hold; v2's bet is that a narrow surface of two nouns, typed so wrong programs don't compile, plus a falsifiable bug-class ledger, can.

---

## 2. The decisive principles

**P1 — Library, not framework.** NMP is something you add to an app you already own. No mandatory AppState, no actor-owns-your-app, no module registration, no lifecycle to adopt. The test is TanStack Query / SwiftData / Room: nobody builds "a TanStack Query app." If any usage requires NMP-shaped scaffolding in the consumer, the design has failed — this is the thesis's own kill condition, and it is judged by a human, on a real app.

**P2 — The reactive filter-binding grammar is the crown jewel.** Every filter field value is a `Binding`:

```
Filter   := { kinds, authors: Binding, tags: {name: Binding}, since/until/limit, search? }
Binding  := Literal(set)
          | Reactive(IdentityField)                          // legal in authors AND tag positions
          | Derived(inner: Filter, project: Selector)
          | SetOp(op: Union|Intersect|Diff, operands: [Binding])   // M0 amendment
Selector := Authors | Ids | Tag(char) | AddressCoord         // CLOSED, introspectable; Tag parameterized
IdentityField := ActivePubkey | …
```

**M0-gate amendments (2026-07-11, both amendments and the gate verdict are recorded in §9).** `SetOp` was added because "my follows minus my mutes" — the most common compound author set — is otherwise inexpressible, which would force apps into hand-maintained expansion and contradict bug-ledger #11. `Reactive` is legal in tag positions (the NIP-29 root is `#p:[Reactive(ActivePubkey)]`). `AddressCoord` does **not** factor into independent field-sets (a set of `a`-coords over-matches as a cartesian product), so a coordinate-projecting `Derived` node either fans out into N outer filters or over-fetches a superset and relies on P5 widen-only local re-filter — the resolver states which, and M1 tests it. The write noun carries a **durability class** (`durable | ephemeral | at-most-once`); ephemeral (typing/presence/NIP-42 auth) and idempotent-RPC (NWC pay) writes are not forced through durable per-relay acks. An engine-internal **encrypt/decrypt capability** sits beside the signer (key co-located in the engine); bug-ledger #12 is scoped to unencrypted content. None of these is a new app-facing noun.

Nesting is bounded (≤3 deep; owner-confirmed no realistic case exceeds it — so no unbounded incremental-dataflow engine, no cycle handling). `Selector` is a closed vocabulary, **never an app closure**: introspectability is the linchpin that lets the engine hash, dedup, coalesce, and route demand (the Electric-SQL "Shape" lesson; the Replicache closure trap). A use case outside the vocabulary extends the vocabulary; it never admits code. At every node the semantics are **replace-not-rebuild** (unchanged members produce zero wire churn) and **recompile-not-reopen** (the outer handle stays open across re-routes; one updated payload, no teardown/reopen race). v1's C5 (`dependent_interests.rs`) is a hardcoded one-level special case of this grammar, welded to kind:3, whose end-to-end path was never truly proven — v2 builds the general primitive and proves it at two different depths.

**P3 — Identity is a pure input.** The app says "the current signer is A… now B." That is the entire identity contract. `Reactive(ActivePubkey)` is whatever the app last set. The engine owns everything *derived* from that input — the account's relay lists, discovered outboxes, follow expansion — and account switch is a re-root of the binding graph that tears the old account's demand down before activating the new (structurally ordered by the resolver, so cross-account leakage has no path). The engine holds no session model, no login flow, no account vector.

**P4 — Routing correctness is the engine's mission, and it is not optional.** Outbox routing is default-on and manual relay lists are structurally refused: there is no `relays:` parameter to omit-guard. Reads route to authors' write relays with a 2-relay-minimum covering set and capped fan-out; writes route to the author's write relays plus tagged recipients' inboxes; indexer relays serve discovery kinds only and are never a content fallback; every relay-bearing fact carries its lane (NIP-65 / hint / provenance / user-configured) and every explicit route carries typed provenance. "Compile filters per relay" means REQ **coalescing** — never author-set sharding.

**P5 — Coalescing may only widen; delivery always re-filters.** The merge lattice is not proven, so correctness must not ride on it (see §6, M2). The structural invariant: a coalesced wire filter must be a superset of every filter it absorbed, and every consumer's rows are re-matched locally against that consumer's *original* filter before delivery. A wrong lattice rule then costs bandwidth, never correctness — the unproven component is demoted from load-bearing to optimization.

**P6 — Truth has a watermark.** A cache miss is authoritative only when a coverage watermark proves the (filter, relay) window complete; a non-empty result is never proof of completeness. Negentropy runs first but only against relays whose NIP-77 support has been probed and cached — unprobed means no. Coverage state is part of what a query returns and what the diagnostic surface renders; it is the difference between "empty" and "unknown," and the API keeps those types apart.

**P7 — A bug-class ledger replaces governance-by-policing.** No principle corpus, no doctrine-lint, no audit treadmill in the new repo. The concrete list of Nostr bugs the design makes structurally impossible (§7) *is* the acceptance criterion; each entry names a type or API-surface mechanism, and each is falsifiable by attempting to write the bug. When a new bug class is discovered, the response is a change to the surface, then a ledger entry — never a lint.

---

## 3. Ownership boundaries

| **NMP owns (engine)** | **The app owns** | **The UI framework owns** |
|---|---|---|
| The event store: validation, id-dedup + provenance merge, replaceable/delete/expiry on insert through one door, persistence, bounded GC | Which queries exist and when — the app's information architecture | Rendering, layout, navigation, animation |
| Binding resolution: expanding `Derived`/`Reactive` bindings, incremental re-evaluation, re-root on identity change | Its own state model, lifecycle, and architecture (MVVM, TCA, plain SwiftUI — engine doesn't care) | View identity and recomposition (driven by the platform-native reactive handle) |
| Subscription compilation & routing: per-relay REQ coalescing, outbox/lane routing, 2-relay-min coverage, capped fan-out, reconnection replay | Who the active signer is (sets it; engine reacts) | Observation scope (Swift task lifetime / Kotlin `WhileSubscribed`) — the refcount edge the engine's teardown-with-grace listens to |
| Sync: negentropy-first vs probed relays, REQ fallback, coverage watermarks | Folding query streams into its own view state; all derivation beyond the closed Selector vocabulary | — |
| Write outbox: durable intents, signing orchestration, route resolution, per-relay ack receipts | Composing what to write; when to write it; how to present in-flight status | — |
| Diagnostic surface: per-relay/per-kind subs, exact filters, event counts, coverage state, lane facts | All presentation: formatting, truncation, locale, fallback display for missing kind:0 (engine emits raw tokens only: hex pubkeys, Unix timestamps, verbatim kind:0) | — |
| Relay transport, capability probing, backpressure | Operator policy: relay config, indexer set, bootstrap choices (app-provided, never hardcoded in engine) | — |

The optional **UI component package** (north-star, not v2-blocking) sits strictly in the third column's territory as a separate per-platform layer that consumes the two nouns exactly as any app would. It never leaks presentation into the engine, and its second job is dogfooding.

---

## 4. Architecture shape

Four planes, one process (in-process is the default deployment; the two nouns are serializable values that never assume shared memory, so a daemon form remains possible later without API redesign).

**The store (data plane).** A local-first replica of events, keyed by id and by replaceable address. One mutating door: insert runs exact-id dedup first (redelivery merges provenance without index churn), then replaceable supersession (newest `created_at`, lexicographically-smallest-id tiebreak). Rows carry provenance (which relays, when) as a field, not a sidecar. Reads serve from cache before network, always; coverage watermarks are stored per (filter-hash, relay) so the store can distinguish *empty* from *unknown*. Bounded, claim-based GC derived from live query demand. This is inherent complexity — Nostr's replaceable/delete/expiry semantics and eventual consistency force it — and it is the plane where most v1 store code is a legitimate harvest candidate (through the import gate).

**The resolver + compiler/router (demand plane).** The heart of the engine and the seat of the grammar. A live query is a value; the resolver expands its bindings into a small dependency graph (nodes = filters, edges = (inner, Selector, target-field), roots = literals and `ActivePubkey`). Events landing in the store that supersede a node's inputs trigger incremental re-evaluation: set-diff at the changed node, replace-not-rebuild downstream, at most one compile invalidation per batch. The compiler then turns the resolved concrete demand set into per-relay wire plans: lane-based routing, coverage solving (2-relay-min, capped), widen-only coalescing, and diffing against the previous plan into surgical CLOSE/REQ deltas. Refcounting lives here too: identical descriptors share one graph; last-observer-drop (debounced) withdraws demand. The O(graph)-per-change cost is acceptable because depth is bounded ≤3 and graphs are small; there is no general dataflow engine here and building one would be introduced complexity.

**The write outbox (intent plane).** Intents are durable on acceptance. Each flows through independent, orthogonal stages — awaiting-capability (signing is one capability, not a lifecycle stage), route resolution via the same lane machinery reads use, per-relay send, per-relay ack — and the receipt streams that status. There is no fire-and-forget publish and no boolean result anywhere on this plane. Private routes carry provenance classes and can only be narrowed, never widened to public relays.

**The diagnostic plane.** Not a debug afterthought: a read-only projection of the other three planes, harvested from the v1 four-lane design (NIP-65 / hint / provenance / user-configured lane facts; reverse relay-coverage: "this relay serves N authors of this query"), extended with coverage/watermark state per (filter, relay). It answers "why did we route this REQ there" and "is this emptiness proven" months after the fact. It is the mechanism by which invisible-by-design routing stays falsifiable.

**The FFI/SDK boundary.** Thin and designed, not codegen residue. Descriptors and receipts are plain serializable values; query results cross as batched row deltas; the platform SDK wraps them in the native reactive primitive (detachable handle first — `AsyncSequence`/`Flow`; view-binding sugar like a property wrapper is a thin adapter on top, never the primary API — SwiftData's retrofit lesson). Errors surface as typed states on handles and receipts. Nothing about the app's architecture crosses this boundary because the engine holds no opinion about it.

**Inherent vs introduced — the discipline.** Inherent to Nostr: eventual consistency over a dynamic relay set, replaceable semantics, outbox routing, subscription minimization, dedup/provenance, offline store, identity switching invalidating derived demand, reconnection replay, FFI chattiness limits. Everything else v1 accumulated — actor choreography as app API, projection registries and rev ladders, per-read-shape lifecycle machines, the doctrine apparatus — is introduced, and v2's architecture must not re-grow it under new names. The tripwire: any time a *second* mechanism appears for expressing read demand or write intent beyond the two nouns, that is the old fragmentation returning.

---

## 5. The falsifier app (MVP spec)

A small, greenfield, **idiomatic SwiftUI app** with its own ordinary architecture, using NMP as a library. Not derived from any existing NMP app (all are contaminated by the thesis under test). Pass condition: a normal iOS developer who knows SwiftData/TanStack patterns could have written it without learning an "NMP architecture." The app is the spec — nothing gets built in the engine that this app doesn't demand.

| Element | What it proves |
|---|---|
| **Multi-nsec login + runtime account switch** | Identity-as-input seam: switch = one input change; engine re-roots the whole binding graph, old account's demand torn down before new activates; no cross-account leakage (verified via diagnostics: zero stale subs for the old pubkey). |
| **User-editable kinds `X` at runtime, queries update live** | Descriptors are values; changing a value recompiles demand without teardown/reopen; reactive composition is real, not launch-time config. |
| **Source mode 1 — "my follows":** `kinds:[X], authors := Derived(kinds:[3], authors:[$currentPubkey] → Tag(p))` | The grammar at **depth 1**, driven through the REAL reducer path (a real kind:3 arriving over the wire → replaceable supersede → binding re-eval → exact CLOSE/REQ delta, no churn on unchanged authors). This retires the v1 smoking gun: C5's contract test used a synthetic stand-in and never exercised the end-to-end path. |
| **Source mode 2 — NIP-29 groups-I'm-in:** inner `kinds:[39002], #p:[$currentPubkey]` → project `Tag(d)` → outer `kinds:[39000,39001,39002], #d := Derived(…)` | The grammar at **depth 2** with an identity root two hops up. Heterogeneous depth is the point: two different shapes through one engine proves a general primitive, not two hardcoded reads. (A third instance — NIP-51 bookmarked relay sets over kind:30002, ranked by bookmark count — is a stretch goal; it is a genuinely new build with no v1 primitive.) |
| **On-demand kind:0 loading for visible avatars** | The generic fallback-loader contract (store calls a loader on miss); demand driven by what the UI actually renders — pay-as-you-go in miniature. |
| **Bootstrap from exactly 2 indexer relays; all content discovered via outbox navigation** | Indexers are discovery-only; outbox routing is default-on and sufficient; zero hardcoded content relays anywhere in engine or app config beyond the two operator-chosen indexers. |
| **Permanent diagnostic screen: per-relay subscription count, exact filters sent, events received per relay per kind, coverage/watermark state per (filter, relay)** | The acceptance test made visible: REQ coalescing observable (N app queries → few wire REQs), 2-relay-min coverage observable, cap respected, negentropy-vs-REQ choice observable, cache-miss authority provable on screen. This screen ships permanently. |

The one judgment no agent makes belongs to the owner, on this app, on a device: *does this feel like a native library, or a framework in disguise?*

---

## 6. The validation plan

**Position on the two open questions, stated up front:**

**Q2 — identity seam vs grammar spike: the grammar spike goes first, and it subsumes the identity seam.** The apparent conflict with v1's actor-owned `IdentityRuntime` dissolves under the import gate: in a greenfield repo there is no `IdentityRuntime` to reconcile — the reconciliation *is* declining to import it. The engine's entire identity state is one input register (active signer handle + pubkey); everything the old runtime derived becomes `Derived` bindings hanging off `Reactive(ActivePubkey)`. That means identity switching is not a separate subsystem to prove — it is literally a root-node change in the binding graph, and the grammar spike exercises it as one of its core assertions. Meanwhile the grammar is the crown jewel, the thing v1 spec'd carefully but never proved end-to-end (the C5 synthetic-stand-in finding), and the component whose failure would invalidate the whole two-noun bet. Kill the biggest risk first; get the identity proof free by construction. Identity-first sequencing would build a seam with nothing to route against and defer the load-bearing unknown.

**Q1 — merge-lattice coalescing: demote it from load-bearing to optimization, then derisk empirically.** The plan does not attempt to formalize a general filter lattice. Instead M2 imposes the widen-only invariant (P5): coalescing may only produce wire filters that are supersets of what they absorb, and every consumer is re-filtered locally against its original filter. Correctness then reduces to two independently checkable facts — (a) each merge rule widens (checked per-rule by property-based tests against a model store: `matches(coalesced) ⊇ matches(f1) ∪ matches(f2)` over generated filters/events), and (b) local re-filtering is exact (ordinary unit tests). A differential oracle runs coalesced vs uncoalesced execution over recorded traffic and asserts identical delivered row sets. The graceful degradation is built in: if a merge rule can't be shown widening, it is dropped and those filters ship as separate REQs — exact-canonical-filter dedup alone is trivially correct and is the floor. The open formalization stops being a correctness risk and becomes a bandwidth-efficiency dial with a measurable threshold.

### Milestones (ordered)

**M0 — Founding gate (thinking only).** *Builds:* new repo (name: NMP); this document as seed; bug-class ledger v1 (§7) checked in; the grammar spec (Binding/Selector, node semantics, depth bound) written precisely enough to refute. *Proves:* the surface is coherent before code exists. *Kill:* the adversarial review produces a required read shape the closed Selector vocabulary cannot express even after vocabulary extension, or shows the two nouns force a third app-facing concept. *Gate:* **Tier A** — propose/refute by two different-model agents, human tie-break. (Tier A applies here and to any later change to the grammar or the noun surface; mechanical milestones below don't re-invoke it.)

**M1 — Grammar engine spike (headless).** *Builds:* minimum-shape binding resolver over an in-memory store with a scripted fake-relay harness — no persistence, no real transport, no FFI. Descriptor hashing/refcounting; graph expansion; incremental re-eval on store change; abstract demand-set deltas (not yet per-relay). *Proves, via the REAL path (event ingested → supersede → re-eval → delta):* depth-1 $myFollows ({A,B,C}→{A,B,D} yields exactly close-C/open-D, zero churn on A,B); depth-2 NIP-29 groups (membership event change cascades one level, re-routes outer, outer handle stays open); identity re-root (ActivePubkey A→B closes the entire old graph before opening the new — teardown-before-activate asserted in order). *Kill:* surgical deltas at depth 2 require per-shape special-casing — i.e., the implementation grows "the kind:3 case" and "the 39002 case" instead of one mechanism — or replace-not-rebuild proves unachievable without O(rebuild) work. That means the grammar is not general and the crown jewel is paste. *Gate:* Tier A sign-off already covered the design; this milestone is settled by its own contract tests (running, headless).

**M2 — Compiler/router + coalescing derisk.** *Builds:* per-relay compilation over M1's demand sets: lane-based routing facts, outbox resolution, 2-relay-min coverage solver with fan-out cap, exact-canonical dedup, widen-only coalescing with local re-filter, per-relay CLOSE/REQ diffing. Property tests + differential oracle per Q1 above. The four-lane diagnostic records come alive here (harvest of the v1 design, through the import gate). *Proves:* N heterogeneous app queries compile to a small per-relay REQ set with no delivered-row divergence from uncoalesced execution; coverage solver respects min-2 and the cap under adversarial mailbox distributions. *Kill:* with coalescing fully disabled (dedup-only floor), realistic falsifier demand exceeds relay REQ/filter limits — meaning correctness would *require* the unproven lattice; the per-relay-compilation approach then needs redesign before proceeding. *Gate:* running (property/differential suites); no Tier A unless the invariant itself must change.

**M3 — Store + write outbox, durable.** *Builds:* persistent store behind the single insert door (dedup-then-supersede, provenance merge, watermarks, claim-based GC); real relay transport (reconnection replay); write intents with per-relay ack receipts and orthogonal sign/route stages; negentropy probing + negentropy-first sync with REQ fallback. Harvest candidates (transport, negentropy, store semantics) enter here through the import gate — re-justified and rewritten, never verbatim. *Proves:* the bug-class ledger's store/write entries hold against live relays; enqueued≠converged is observable in receipt streams; unprobed relays never get negentropy. *Kill:* none thesis-level — this is execution risk, not bet risk; failures here are fixed, not abandoned. *Gate:* running (integration harness against `nak serve`-class local relays).

**M4 — Swift SDK boundary.** *Builds:* the FFI seam + Swift package: detachable `AsyncSequence` query handles with deinit-tied demand drop and teardown-with-grace; `@Observable` snapshot adapter on top; async write with receipt stream. Minimum surface — exactly what M5 consumes. *Proves:* the two nouns survive the boundary as values; no engine concept leaks that isn't one of the two nouns plus diagnostics. *Kill:* delivering native ergonomics forces the SDK to grow app-lifecycle machinery (scene-phase hooks, mandatory containers) — the library thesis failing at the boundary rather than the core. *Gate:* Tier A on the public SDK shape (it's the product's face and is expensive to re-cut), then running.

**M5 — The falsifier app (the thesis gate).** *Builds:* the §5 app, complete, on simulator and device. *Proves:* everything in the §5 table, plus the qualitative pass. *Kill — the pre-committed thesis kill:* after honest effort, the app still needs NMP-shaped scaffolding, or a normal iOS developer couldn't have written it from SwiftData/Query knowledge alone. If this fires, the two-noun bet is broken: stop, re-open the exploration, do not patch around it. *Gate:* **Tier B** running falsifier on device + the owner's human judgment. This is the only milestone where the human call is the gate itself.

**M6 — Consolidation & second platform.** *Builds:* Kotlin/Flow SDK as the cross-platform proof; watermark-driven cache-authority demonstrated cold-start-offline; ledger review pass (attempt to write each bug; record why each attempt fails to compile or reach the wire). *Proves:* the SDK layer is a thin per-platform adapter, not a per-platform rewrite. *Kill:* Kotlin requires reshaping the core surface — the boundary was Swift-shaped, not platform-neutral. *Gate:* running.

Model tiering throughout: design/reconciliation Fable/Opus; mechanical build-out Sonnet/Haiku; refutation always an independent Opus. Old repo stays alive for its consumers; harvest is one-way, gated, provenance-recorded per subsystem.

---

## 7. The bug-class acceptance ledger (v1)

The replacement for the principle corpus. Each entry: the bug, and the **structural mechanism** — a property of types or API surface — that excludes it. Falsification standard: to claim an entry holds, an agent attempts to write the bug against the public surface and records why the attempt cannot compile, cannot reach the wire, or cannot corrupt state. Lints are not admissible mechanisms.

1. **Stale replaceable event retained.** One mutating store door; replaceable supersession runs *inside* insert (dedup-first, then supersede; newest created_at, lexically-smallest-id tiebreak). No public index/storage setter exists; reads by address can only return the current winner.
2. **Lost or leaked subscription.** Wire subscriptions are derived exclusively from the live-query demand set; there is no open-a-REQ API. A leak therefore requires a live handle (visible, countable in diagnostics); a loss requires dropping a handle (the app's explicit act). Handle drop → refcount edge → demand withdrawal, with debounced grace.
3. **Wrong-relay routing / manual relay lists.** No `relays:` parameter exists on reads or writes. Relay choice is compiler output from lane-typed facts; the only relay inputs are role-tagged operator config (indexer/user-configured lanes), which the compiler treats as policy, not as a route override channel.
4. **Uncapped fan-out.** The relay set for a demand set is the output of the coverage solver, whose cap is a required parameter with an engine default — never an accumulated union of per-author relay lists. There is no code path that connects to a relay outside a solver-produced plan.
5. **Dedup or provenance loss.** Insert merges provenance on duplicate id before any other processing; provenance is a field of the stored row. Ids and signatures are never re-derived post-verification. No API returns an event without its provenance being retained in the store.
6. **Private-event republish.** Every explicit route carries a typed provenance class; routes derived from private lanes (e.g. resolved DM inboxes) admit only narrowing overrides — the type of a private route has no widen operation. Unroutable private recipients fail closed (typed error), never fall back to public relays.
7. **Cache-miss treated as empty (and its inverse, redundant over-fetch).** Query results carry coverage state as a type: rows plus a coverage variant (`Unknown` vs `CompleteUpTo(watermark)`). "Not found" is only constructible from a proven watermark; conversely, the sync planner consults the same watermark before re-fetching a proven-complete window.
8. **Assuming NIP-77 support.** Negentropy sync requires a probed-capability token for the relay, obtainable only from the prober's cache. Unprobed relays cannot be passed to the negentropy path — the parameter type doesn't accept them; they get plain REQ.
9. **Enqueue treated as converged.** Write acceptance returns a receipt whose status is a stream with per-relay terminal acks; no publish API returns `void`/`bool`. Any "is it sent?" question can only be answered by reading receipt states, which distinguish accepted / signed / routed / sent(relay) / acked(relay).
10. **Multi-account desync / cross-account leak.** All account-scoped demand hangs off `Reactive(ActivePubkey)` — there is no second place account context lives. Switch is a root replacement whose resolver-ordered execution closes the old graph (reverse-of-open, exactly-once) before opening the new; a stale account's callbacks have no surviving subscription to deliver into.
11. **App owning interest expansion.** `Derived` bindings resolve inside the engine; the query API returns final rows, never the expanded intermediate sets, and `Selector` is a closed vocabulary — there is no seam through which the app can observe, cache, or hand-maintain an expansion (diagnostics expose expansions read-only, off the data path). The canonical NDK-era "app watches kind:3 and re-issues REQs" bug has nothing to attach to.
12. **Presentation in core.** The engine emits raw tokens only — hex pubkeys, Unix timestamps, verbatim kind:0 content. No display helper exists in the engine crate; no formatted-string field exists on any FFI type. Formatting is unreachable from the engine because the vocabulary to express it is absent, not because a lint forbids it.

The ledger is append-only in spirit: a newly discovered bug class demands a surface change plus a new entry, and an entry whose mechanism erodes (e.g. a new API adds a `Vec<RelayUrl>` anywhere) is a red build, because the falsification tests for each entry live in CI.

---

## 8. Risks, unknowns, and what would change the recommendation

**The grammar might not survive contact (highest-value risk, killed first).** M1 exists because C5's history is a warning: carefully spec'd, partially built, never proven on the real path. If surgical deltas at depth 2 need per-shape code, the "general engine" is a fiction and we'd be honest to fall back to a small set of blessed derived sources — a much less interesting library, but a shippable one. This is why M1 is headless and cheap: the bet is falsified for the price of a spike, not a rewrite.

**Coalescing efficiency is unmeasured.** The widen-only demotion makes coalescing safe, not necessarily sufficient. If real falsifier demand under dedup-only exceeds relay limits *and* widening rules can't close the gap, per-relay compilation needs a rethink (M2's kill). Current evidence (relays accept large author REQs; no sharding needed) suggests this won't fire, but it's the plan's least-grounded empirical assumption.

**Second-system risk is real and only partially mitigated.** The import gate protects against re-importing wrongness but also taxes re-importing hard-won operational lessons (transport edge cases, incremental-emission profiling, reconnection behavior). The mitigation is that M3 explicitly treats v1 transport/store/negentropy as first-class harvest candidates with recorded provenance — re-earned, not re-invented. If M3 drags badly, the correction is to widen the harvest, not to abandon the surface.

**The qualitative pass is one person's judgment.** M5's gate is deliberately human and deliberately vibes-adjacent ("native library or framework in disguise?"). That's a feature — no agent can make the call — but it means the thesis verdict has no numeric threshold. The pre-committed kill condition ("needs NMP-shaped scaffolding") is the falsifiable half; the owner should resist rationalizing scaffolding as "just helpers" at that moment.

**Genuinely unresolved (framed, not forced):**
- *Where does teardown-with-grace debounce live per platform* — engine-global default vs per-handle knob? Trivial to change pre-v2; deferred to M4 evidence.
- *Web/wasm* — likely out of v2; confirm before anyone starts a TS SDK. The two-noun surface is wasm-compatible by construction (serializable values), so deferral costs nothing structural.
- *Does light use run a background sync loop?* Almost certainly yes (offline persistence wants it); the P1 objection is to architecting *around* it, not to its existence. If the falsifier shows even a background engine feels heavy for the podcast-player case, a lazier "sync on demand" mode becomes a v2.x knob — not a surface change.
- *NIP-51 relay-set mode* (falsifier stretch goal) is a new build with no v1 primitive; if M5 is at risk, it drops first — depth-1 + depth-2 already prove the grammar's generality.

**What would change the recommendation wholesale:** M1's kill firing (grammar not general) or M5's kill firing (library thesis broken). Everything else in this plan is adjustable in place. Those two are the bet; the plan is built so each is falsified as early and as cheaply as possible.

---

## 9. M0 gate verdict (2026-07-11)

**PASS — no kill.** Two independent Opus agents (an adversarial refuter tasked to break the grammar, and a completeness auditor cataloguing ~25 real Nostr read/write shapes) both concluded the two-noun surface holds: no read requires an app closure, and nothing forces a genuine third app-facing concept. NIP-45 relay-COUNT is the only read that isn't a live query, and it is a *deliberate scope exclusion* (count locally over a coverage window), not a missing noun.

The pass was **conditional on amendments**, all applied above and folded into M1:

- **`SetOp(Union|Intersect|Diff, [Binding])`** — the refuter's sharpest finding: without set-difference the grammar could not express mute-filtered follows and thereby *contradicted its own bug-ledger #11*. Now consistent.
- **Write durability class** (`durable|ephemeral|at-most-once`) — ledger #9's "no void/bool ever" was over-strict for ephemeral and idempotent-RPC writes.
- **Engine-internal encrypt/decrypt capability** co-located with the signer — a real hole: DMs and private lists need decryption where the key lives, or identity-as-input breaks. Ledger #12 scoped to unencrypted content.
- **`Reactive` legal in tag positions; `AddressCoord` fan-out/over-fetch escape stated and M1-tested; `Tag(char)` parameterized; `search` Filter field reserved.**
- Routing needs lane vocabulary beyond NIP-65 (group-host/NIP-29, DM-inbox/kind:10050, search-relay) — an M2 concern, noted so it isn't discovered late.

The full agent findings live in the design record. This gate is called clean; M1 proceeds.
