# NMP first-principles architecture exploration

- **Session:** NMP first-principles redesign exploration
- **Date:** 2026-07-10
- **Context:** nostr-multi-platform repo; owner asked for a from-scratch evaluation of architectural directions against the north star (misuse-resistant multi-platform Nostr framework). Explicitly not assuming the current architecture, standalone-ness, or Dioxus integration.
- **Status:** converged and promoted into `docs/VISION.md` plus the focused
  contracts under `docs/design/`.
- **Reading rule:** this file preserves the chronological exploration. Earlier
  proposals are evidence, not current normative guidance; the promoted frame at
  the end names the conclusions that supersede them.

## Core question
What architectural shape best delivers "one clear, reliable way to handle the hard Nostr concerns" such that misuse is prevented structurally, not by doctrine/lints/audits?

## Current working model
The single Rust core is the right substrate; the app-facing programming model is the suspect part. Candidate synthesis: framework-agnostic engine with a **declarative live-query + typed write-intent** programming model (a "Nostr database"), consumed through **designed platform SDKs** with scope-bound handles. The TEA/actor becomes an internal implementation detail, not the app API.

## Observations (grounded)
- `docs/aim.md` §1 already states the structural bar: bug classes "ruled out by the type system, actor ownership, and public API surface... not merely documented as a footgun or caught by a linter."
- Yet the repo runs a large policing apparatus: 46-principle conformance corpus, doctrine-lint, file-size gates, recurring multi-agent audits. Violations recur (display-separation ×27 repetitions; feed render-policy leak #3082).
- Read-lifecycle machinery keeps accreting per read shape: concept reads, demand sets (#3034), feed sessions, Trellis reconcilers (#3115/#3116 exist because reads kept being hand-rolled). Evidence the read model isn't general.
- Apps register Rust modules/projections *inside* the kernel's extension seams — the internal composition surface is wide, and that's where doctrine-lint lives.
- 5 external consumer apps (chirp, 29er, hl, podcast-player, win-the-day) are SwiftUI/Kotlin-native — any UI-framework-coupled direction (Dioxus) abandons them.

## Inherent vs introduced complexity
- **Inherent (Nostr + cross-platform):** eventual consistency over a dynamic relay set; replaceable/delete/expiration semantics; outbox routing; subscription minimization (N views → few REQs); dedup + provenance; offline-first store + write outbox; identity switching invalidating derived state; reconnection replay; FFI chattiness limits.
- **Introduced (current NMP):** actor command choreography as the app programming model; projection registry + rev ladders + ProjectionCache codegen; per-read-shape lifecycle machines; FlatBuffers sidecars; the doctrine/audit apparatus itself.

## Alternatives considered
- **A. Refined domain runtime (current shape):** proven, shipping; risk = super-linear kernel-concept accretion, social not structural enforcement.
- **B. Declarative query engine ("Nostr database"):** apps hand the engine *data* (query descriptions + write intents), never code-in-kernel. Subscription compilation/routing/dedup/replay become planner outputs. Complexity concentrates in one hard query planner. Risk: expressiveness ceiling → escape-hatch pressure; floor = raw-filter live query, still lifecycle/route-managed.
- **C. Dioxus-integrated library:** lifecycle-for-free is elegant but couples core to one framework's fate, kills native consumers; only viable as a *binding* over B/D.
- **D. Framework-agnostic core + designed platform SDKs:** what NMP is de facto becoming (UniFFI facade + generated caches), but SDKs unowned/codegen-residue today. Structural enforcement should live in the SDK types (scope-bound query handles).
- **E. Local daemon/NIP-01 proxy:** dead on iOS process model; noted, rejected.

## Emerging direction (not yet decided)
D wrapping B. Keep single Rust core + internal actor; replace app-facing model with live queries + typed intents; promote Swift/Kotlin/TS SDKs to designed products; app-kernel Rust (chirp-style domain logic) becomes a *consumer* of the engine's query API, not a co-resident with registry privileges.

## Open questions
- Expressiveness: can chirp/29er/Marmot read shapes all be expressed as queries + pure app-side derivation? (Marmot/MLS state machines are the stress test — they are not pure folds over the store.)
- Where do stateful protocol modules (NIP-46 broker, MLS) live in a query-engine world — engine plugins with bounded contracts?
- Migration economics: 5 consumers + device-proven Marmot vs second-system risk.

## Risks
- Second-system effect; losing operationally hard-won lessons (relay transport, incremental emission profiling).
- Query planner becomes the new god component.

## Proposed falsifying prototype
Live-query facade over the existing kernel for two dissimilar read shapes (feed-with-reply-digests, profile resolution), consumed from one chirp SwiftUI screen via a scope-bound Swift query object; compilation/routing/close-on-drop derived automatically, zero doctrine-lint rules needed. Pass = second shape reuses engine with no new kernel concepts.

## Second agent's proposal (2026-07-10) + my critique
Second agent (did repo/web fetch — cited builder.rs, Trellis lib.rs, issue #3143) proposed:
- Same outer shell: headless framework-agnostic kernel + Rust/Swift/Kotlin/JS/Dioxus SDKs. **CONVERGES with my D.**
- Central abstraction: "scoped application intent as one durable resource" (unifies read+write into "resource").
- Recommended interior: **tri-plane** — control plane (Trellis-like deterministic graph), data plane (local-first event engine), effect plane (acknowledged bounded workers), + SDK plane.
- Strongest alt: database-first sync engine (= my B).
- Prototype: active-account timeline in shadow mode, one UniFFI host. **NEARLY IDENTICAL to mine.**

**Agreements (it made me update):**
- ACK-effects plane: "queue acceptance ≠ convergence" (desired/planned/queued/applied/observed are distinct states). Real gap, grounded in #3143 + snapshot-registry deadlock memory. I MISSED this.
- "Invalid states unrepresentable" concrete list — better than my abstract treatment; that IS the structural-enforcement mechanism made tangible.
- Product-neutrality (no blessed display policy) = #3082 lesson. Agree.

**Disagreements:**
1. Tri-plane as *recommendation* is the current fragmentation (registries+sessions+reconcilers) RELOCATED INWARD and renamed, not eliminated. Suspicion: it's "promote Trellis from private per-session substrate to app-wide control plane." Memory says Trellis is private, in-memory, per-session, zero-persistence, tx-cost O(total graph state). Promoting it makes that O(graph) cost application-wide. The agent notes the constraint then waves it through.
2. Splitting control/data planes REINTRODUCES the two-commit-order consistency problem the single actor solved by construction. Agent lists "careful consistency boundaries" as a caveat but presents plane-splitting as a benefit. Net could be introduced complexity, not removed.
3. Central abstraction unifies read+write into "resource" — blurs the rigid read/write split aim.md §4.3 insists on (reads=replayable, no terminal state; writes=durable, terminal receipt). SDK plane half-walks-this-back.
4. Neither of us solved the EXPRESSIVENESS question (can all read shapes + MLS express declaratively?). Tri-plane doesn't answer it — arguably worsens it by requiring every intent as control-graph state.

**What it did NOT propose:**
- Migration cost: 5 master-tracking consumers (chirp/29er/hl/podcast-player/win-the-day) break on any surface change; tri-plane = lockstep rewrite of all 5. Its recommendation is the most expensive path and it doesn't price it.
- The cheapest north-star-satisfying move: keep the actor internals AS-IS, change only the app-facing surface (freeze seams + live-query SDK). Misuse-resistance is an API-surface property; internal "registries behind a facade" is only a problem IF it leaks, which the SDK boundary prevents. Agent conflates "app-facing model fragmented" (real) with "internals must be re-planed" (non-sequitur).
- Preserving hard-won operational lessons (relay transport specifics, ADR-0070 incremental emission = measured 18%). Rewrite regresses these.
- Marmot/MLS stress test: MLS state is NOT a pure fold over the store (stateful ratchet, own persisted crypto state). Where does it live in the tri-plane? Gap in BOTH analyses; agent claims more completeness than it has.
- Whether NMP should even OWN whole-application-lifecycle correctness vs. just storage+sync correctness. That's the actual strategic fork; asserted not argued.

**Convergence signal:** we agree on shell + prototype, disagree on interior. The interior (query engine vs tri-plane) should be DECIDED BY the prototype outcome, not committed now.

## Owner constraints (2026-07-10) — DECISIVE
1. **Apps don't work / "they all suck."** → kills the migration-cost argument. Nothing to preserve. "Just wrap the current kernel to protect 5 consumers" loses its justification.
2. **Internals too large/complex; podcast-player barely uses nostr yet must buy the whole framework. "An app shouldn't need to buy a whole way of architecting itself just to talk to nostr."** → THE reframe. NMP must be a **library/engine you talk to**, NOT a **framework you build your app inside**. Adoption must be pay-as-you-go / subtractable. No mandatory AppState, no mandatory actor-owns-your-app, no mandatory module registration.
3. **Don't care about Marmot/MLS (shouldn't have opened that door yet).** → removes the falsifying stress test. Pure-fold / declarative-query model is now much more viable; expressiveness ceiling is a minor concern.
4. **Storage-only is wrong; outbox + routing ARE real work NMP should supply.** → mission = storage + synchronization + routing correctness. Not a bare local DB. Not whole-app-lifecycle either.

## What I got wrong (corrected)
- Overweighted migration cost (#1 kills it).
- Overweighted MLS as falsifier (#3 removes it).
- Underweighted library-vs-framework axis (#2 makes it central).
- Posed a false binary (storage-only vs whole-lifecycle); real answer = storage+sync+routing, NOT app-lifecycle (#2+#4).

## Sharpened direction (converging)
NMP = **headless, embeddable Nostr sync-and-routing engine**. Central abstraction: **declarative live query + typed write intent** over a **local-first synchronizing replica**.
- NMP OWNS (data plane + network correctness): validation, dedup, replaceable/delete/expiry, provenance, persistence+replay, subscription compilation/sharing, outbox/relay selection, request+event routing, reconnection, backpressure, durable write outbox with acknowledged receipts, query leases (scoped remote demand).
- NMP does NOT own: the app's state model, lifecycle, UI architecture, or "who is active."
- **Identity seam:** app owns "who" (active account is an INPUT); engine owns the DERIVED routing state (that account's relay list, discovered outboxes) + all fetching.
- **Delivery:** queries surface as the platform's native reactive primitive (Swift AsyncSequence/@Observable, Kotlin Flow, JS async iterator/signal, Rust Stream). App folds streams into its OWN state. NMP hands you streams, not an AppState to render.
- Scales down (podcast-player: 2 queries + 1 intent, otherwise a normal SwiftUI app) and up (full client: many queries + heavy outbox routing). Cost of adoption ∝ use. That IS the pay-as-you-go property #2 demands.

## Key insight — the fix in one sentence
The current architecture CONFLATED two things: the **engine** (store/routing/sync — genuinely valuable) and the **app framework** (TEA/actor/AppState — the thing podcast-player shouldn't have to buy). Separate them: **engine = mandatory minimal library; app-kernel = optional topping** for greenfield full clients. aim.md's "one-shot a working app" goal lives in the OPTIONAL layer, not the foundation.

## Lifecycle NMP manages (much smaller than tri-plane)
Not the application's lifecycle. Only: **query leases** (live query = scoped resource; drop → demand drops → subscription closes) + **write outbox** (intents are durable, acknowledged: enqueued ≠ converged, per #3143). No Trellis-as-app-control-plane needed.

## Still open
- Outbox routing needs identity context without owning session model → the "identity is an input, routing state is derived" seam needs proving.
- Does light use still run a background replica/sync loop? (Likely yes — podcast-player wants offline persistence.) The objection in #2 is "architect around it," not "a background engine exists." A background engine you query is fine.
- Second-system / rebuild risk vs. refactor-in-place — #1 lowers preservation value but doesn't mandate greenfield.

## Prototype correction (2026-07-10) — falsifier must be a CLEAN app
Owner: can't validate "NMP shouldn't force you to be an NMP app" by driving an existing NMP app — every current app is contaminated by the thesis-under-test. Analogy owner keeps returning to: TanStack Query (web) / SwiftData @Query / Room+Flow (iOS/Android) — libraries you ADD to a normal app; nobody builds "a TanStack Query app."
- **Falsifier = a small, greenfield, idiomatic SwiftUI app** with its OWN state/architecture, that uses NMP as a library for a real Nostr surface (timeline + compose). Pass = a normal iOS dev could have written it knowing only SwiftData/Query patterns, with no "NMP architecture" learned. Fail = it needs NMP-shaped scaffolding.
- NOT podcast-player-as-it-exists. Reface still fine as the disposable backing; the CONSUMER must be clean.
- SwiftUI first (most active surface, XcodeBuildMCP tooling here, cleanest native-library comparison). Compose second = the real cross-platform proof.

## Daemon vs in-process — RESOLVED as non-fork
Owner: "honestly no idea." Resolution: it's a deployment detail, not an API fork. The two nouns MUST cross the Rust↔native FFI boundary regardless (iOS/Android), so they're already serializable and must not assume shared process memory. Hold that one rule and: **in-process is the default first form** (how SwiftData/Room/TanStack all work); daemon becomes possible later for desktop/server with no API redesign. Don't decide now.

## Prior-art survey findings (2026-07-10) — two-noun model CONFIRMED
Surveyed TanStack Query, SwiftData @Query, Room+Flow, Electric-SQL, LiveStore, Replicache, PowerSync, Apollo, RxDB/WatermelonDB.

**Linchpin decision:** the live-query DESCRIPTOR must be a plain `Hashable`/value (filter/kinds/authors/etc.), NOT a closure or opaque fn. That same value doubles as the demand-routing key + refcount key — this is exactly how Electric's "Shape" makes query==wire-demand with no translation layer. Closures-as-descriptor (Replicache) can't route server demand → fatal for us. This single decision is what lets one noun be both "what the UI wants" and "what we fetch."

**Two nouns survive; the recurring THIRD concept is engine-INTERNAL, not app-facing:** every mature system has a normalized entity/identity cache under both reads+writes so one write ripples into every live query with no app-level invalidation. For Nostr this is native (events keyed by id/address; replaceable by address) — NMP's store already does it. Engine must own it; it does NOT become a third app noun. **Two-noun model holds.**

**Table-stakes ergonomics to match:** (1) descriptor-as-data (refcount/dedupe/introspect), (2) cold-until-observed + refcounted sharing + teardown-with-grace (Room `WhileSubscribed(5s)` — the refcount-to-0 edge IS our "drop remote demand" signal; matches aim.md §4.2 warm-view), (3) write as named typed op distinct from cache-mutation, (4) ack/settle status without polling (matches enqueued≠converged / #3143).

**Concrete platform shapes to steal:**
- Swift: live query as `AsyncSequence<[T]>` + thin `@Observable` snapshot wrapper (mirror SwiftData's ResultsObserver split — ship a DETACHABLE handle first, view-binding is a thin adapter; SwiftData's @Query-view-only was retrofitted years later, don't repeat). Demand-drop tied to handle deinit (ARC) + explicit `close()`. Write: `func write(_ intent) async throws -> WriteReceipt` + optional `AsyncSequence<WriteStatus>`.
- Kotlin: live query as cold `Flow<List<T>>` (caller does `.stateIn(scope, WhileSubscribed(5000))` — reuse Room idiom verbatim). Write: `suspend fun write(intent): WriteReceipt` + optional `Flow<WriteStatus>`.
- AVOID: implicit call-site scoping (LiveStore deprecated `useScopedQuery` — queries must be shareable VALUES).

**Consequence for current NMP work:** the "descriptor → refcounted subscription" the API needs is the SAME mechanic as NMP's demand-reconciler family (reconcile_read_demand_set, DependentDemandReconciler, kernel ReplaceDependentInterestSet, reconcile_feed_author_refs — 4 hand-specialized copies; #3115 = 5th). The redesign gives #3115/#3116 (unify the reconciler family) its telos: collapse those into the ONE internal primitive the app-facing descriptor exposes. Redesign is NOT orthogonal to in-flight work — it's its endpoint. Strengthens the reface path: we're EXPOSING existing mechanics, not inventing.

## Process design (2026-07-10) — how the migration will work
Owner's stated ingredients:
1. New repo; harvest useful stuff IN from old NMP, never edit-in-place.
2. YAGNI-but-correct. Two agents sign off on the thinking being correct.
3. Decisions justified pragmatically, minimum-shape, without relaxing engineering rigor.
4. This is a code migration. Nothing formalized until v2.0 ships; not before Aug 2026.
5. Reusable NMP UI components (conceptual, like before) → north-star, not priority.

**Central risk owner's list doesn't yet cover:** "harvest useful stuff in" vs "the old design was wrong" is a live tension — harvesting verbatim re-imports the wrongness. The whole failure mode of old-NMP was **governance-by-policing** (46 principles + doctrine-lint + recurring audits) compensating for an API that admitted wrong programs. If the new repo recreates that apparatus, we've failed regardless of the two nouns.

**Missing process ingredients (my additions):**
- **The falsifier app IS the spec — build consumer-inward, not engine-outward.** Nothing gets built that the clean SwiftUI app doesn't demand. This is what gives YAGNI teeth: "the app needs it" is the only admission ticket. Prevents rebuilding subsystems in isolation that never integrate.
- **Import gate: nothing crosses the boundary verbatim.** Old code enters as REFERENCE, is re-justified against the two-noun model, and is rewritten to fit. Even hard-won bits (transport, negentropy, incremental-emission) are re-earned, not assumed. Provenance recorded per imported subsystem.
- **A bug-class ledger REPLACES the principle-corpus/lint police force.** The concrete list of Nostr bugs the design must make structurally impossible (stale replaceable, lost subs, wrong relays, dup, leaked signing, multi-account desync, enqueued-treated-as-converged) IS the acceptance test. Correctness lives in types + this falsifiable ledger, NOT in a lint that patrols a wide seam.
- **Two-tier gate, not one.** (a) THINKING review: two agents, DIFFERENT models, adversarial (one proposes, one must try to REFUTE), human tie-break — required only for foundational/irreversible decisions, not mechanical ports (correlated agreement between similar models ≠ correct). (b) RUNNING falsifier: some claims can only be settled by driving the app on sim/device (verify-the-running-result). Thinking-signoff necessary but insufficient.
- **Pre-committed kill conditions, not just approval.** Per milestone: what running evidence proves it AND what evidence would abandon the two-noun bet. E.g. if a light app still needs NMP-shaped scaffolding after honest effort → thesis broken, reconsider.
- **v2 scope fence (explicit OUT list).** MLS/Marmot OUT (owner cut it). Wallet OUT (already post-v1). Web/wasm: likely deferred, confirm. UI components = north-star OPTIONAL package, not v2-blocking. Written down so scope can't creep.
- **Old repo stays alive, one-way harvest only.** Current consumers/fixes continue in old-NMP until v2 ships; new repo is greenfield; NO bidirectional-sync obligation. Old→new is reference extraction, not a live branch merge.
- **New-repo hygiene from day one:** canonical surfaces decided up front (design doc + issues), a provisional-decision log where EVERYTHING is marked "current best, not final until v2" — and the freedom that implies: **no self-compat obligation to our own pre-v2 code; break it freely.**
- **UI-components tension resolved:** a reusable-UI package is a SEPARATE opt-in per-platform layer ON TOP of the engine (like the optional app-kernel topping), consuming the two nouns exactly as any app would — never in the engine, never leaking presentation down (preserves display-separation P2). It dogfoods the engine; that's its second value.
- **Role/model tiers:** design/reconciliation = Fable/Opus; mechanical ports = Sonnet/Haiku; verify/refute = independent Opus. Human owns the irreplaceable judgment: "does this feel like a native library, or a framework in disguise?" — no agent can make that call.

**Open process questions:**
- Repo name / does it stay "NMP"? (owner keeps saying NMP conceptually)
- What graduates this exploration note → the new repo's founding design doc, and when?
- First vertical slice concretely: timeline + compose is the falsifier surface — is that THE slice, or is login/identity the true first slice (identity-as-input seam is unproven)?

## MVP spec (2026-07-11) — the falsifier app, owner-specced
Name: **keep NMP.** First slice concretely:
- Log in with nsec, or MULTIPLE nsecs (→ proves account switching).
- See kinds X, where X is user-changeable at runtime and the queries change in realtime from observing the input (→ reactive composite queries).
- Two source modes:
  1. toggle "my follows" → `authors:[$myFollows]`.
  2. select from NON-hardcoded relays — relays come from the kind:10002 of the people the user follows — OR from bookmarked relay sets (NIP-51), sorted by how many people bookmarked each relay.
- **Diagnostic surface (first-class):** exactly how many subscriptions + what filters were sent PER RELAY, and how many events received PER RELAY and PER KIND.
- Bootstrap: start with ONLY two indexer-role relays; navigate all content discovery through relay outbox.

What it proves (the hard parts NMP must provide):
- account switching
- **automatic expansion of composite/reactive queries** — `kinds:[x], authors:[$myFollows]` (or relay-pinned). The APP does not know how `$myFollows` is expanded or recalculated when a change arrives through the subscription that was opened by the mere fact that an interest in `$myFollows` was expanded. (Owner: "I speced this out very carefully" — unsure if old impl executed it well. THIS is the load-bearing primitive: a live query whose author-set is the live output of another live query; teardown/re-route on change.)
- on-demand kind:0 loading — react to loading avatars in the feed when needed to display.
- **filter compilation at the relay level** (like NDK) — coalesce similar filters into one REQ per relay.
- **minimum 2-relay intersection per author or p-tag pubkey** — don't connect to hundreds of relays just because a few people list dozens. Cap fan-out.
- efficient cache; **preferred negentropy** (NIP-77) over naive REQ where possible.

Known repo grounding (from injected context): "no per-relay author-set sharding needed (relays accept large author REQ)"; "hardcoded operator relays/seed pubkeys/bootstrap URLs belong ONLY in app code, never nmp-core/nmp-defaults"; applesauce/NDK relay-model research already exists (docs/research/{applesauce,ndk}-app-relay-model.md); "make outbox-aware routing the DEFAULT, not opt-in."

## Non-negotiables ledger (agent-mined, 2026-07-11) — IN PROGRESS (1/5 back)
### Relay / routing / outbox (agent 1)
- **Outbox routing ALWAYS on; manual relay lists are structurally refused, not documented.** D3: no `relays:` param on writes; picking relays per-publish is blocked by the API's absence, not a warning. (docs/product-spec/doctrine.md:97-106, docs/builder-guide/10-outbox-routing.md, SYNTHESIS-app-relays.md §2/§6)
- **Min-N coverage per author + capped fan-out.** goal=2 relays/author, minimum covering set, greedy max-coverage post-compile mutator (`select_optimal_relays`/`apply_selection`). Ported from NDK `chooseRelayCombinationForPubkeys`. (docs/design/relay-roles.md §7)
- **"Compile per relay" = REQ/filter COALESCING, NOT author-set sharding.** Relays accept large author REQ; do not split authors per relay. Merge-lattice "Rule 11". (docs/design/relay-roles.md §4.5)
- **Zero hardcoded relay URLs in core/nmp-defaults.** All relay URLs app-provided; bootstrap-discovery relays are "NOT a routing default". Every relay carries an explicit, operator-visible, inspectable ROLE/LANE. (builder-guide/14a, relay-roles.md)
- **Indexer relays = discovery-only (kind 0/3/10002), NEVER content fallback.** Read symmetry for discovery kinds only. DMs fail closed to resolved inboxes (`PrivateRecipientUnroutable`).
- **Publish provenance class** (from injected context): every explicit relay route carries a typed provenance enum — automatic / host-pin / verified-private-inbox / manual / imported / diagnostic. Override may only NARROW a private fan-out, never widen to a public relay.
- **MVP GAPS (agent 1):** (a) NIP-51 bookmarked relay sets (kind:30002) explicitly DEFERRED/unconsumed in v1 (docs/design/intent-routing/nip51-facts.md) — the MVP's "pick from bookmarked relay sets ranked by bookmark count" mode has NO existing primitive → NEW build. (b) 2-relay-min exists for NIP-65 outbox authors but NOT for "relays from followed users' 10002s ranked/capped" → new wiring on case_a_authors/case_i_app_relay.
- **TRAPS:** opt-in outbox (applesauce's "forgottenness farm"); one relay-set doing 4 jobs w/ hidden lanes (NDK #175-class); auto-merging signer-suggested relays (record, never merge); silent snapshot revert to a hardcoded fallback relay (just-fixed real bug); indexer-as-content-fallback; DM widening on override.

### Cache / store / negentropy (agent 3)
- **Replaceable-on-insert via ONE mutating door** (`EventStore::insert`; no public index/storage setter). Order: exact-id dedup FIRST (redelivery merges provenance, no index churn), THEN replaceable supersede. Winner = newest `created_at`, tie-break **lexicographically smallest id**. Kinds {0,3,10000-19999} by (pubkey,kind); {30000-39999} by (pubkey,kind,d). (doctrine D9/D4, aim.md, 08-eventstore.md)
- **Negentropy (NIP-77) FIRST, REQ second — DEFAULT not opt-in.** D2: "not an optimization you opt into later." BUT only when relay NIP-77 capability is PROBED+cached AND the filter's result surface isn't statically tiny (small exact sets stay on plain REQ). **Unprobed capability = conservatively NO NIP-77, never assumed.** (doctrine D2, 13-sync-engine.md)
- **Cache-miss is authoritative ONLY when a coverage watermark proves the (filter_hash, relay) window complete.** "A non-empty result is never proof a query is complete; only a covering watermark turns a miss into 'not found'." → MVP diagnostic should surface COVERAGE/watermark state per (filter,relay), not just event counts — coverage is what proves cache correctness.
- **Dedup by id + provenance merge, id/sig never re-derived.** `ProvenanceRow{sources: Vec<Entry>}` (relay, first/last-seen, deterministic-sticky-primary); duplicate delivery updates provenance without rewriting primary row. Byte-stable across redeliveries.
- **Cache-serve before network; render never gates on network (D1).** Backfill streams in behind the cached render; no spinner over cached rows.
- **Bounded explicit GC; claim-based pin set** derived transiently before each pass from live views + component claims + in-flight publishes + coverage floor. `gc_step` actor-only, never FFI. Prod default `max_total_events=MAX` (no silent drop).
- **On-demand kind:0 for avatars = an instance of the generic FALLBACK-LOADER contract**, not a named special rule. (Confirms it's the general "store calls user-provided loader on miss" primitive — good, no bespoke machinery.)
- **MVP GAPS (agent 3):** no `StoreQuery` variant for relay-provenance queries yet (`replay_relay_provenance` deferred); provenance distinct-relay CAP (old "32-relay" bound) may be superseded/undocumented — confirm in code.
- **TRAPS:** stale replaceable retained (bug-extinction #1); cache-miss disguised as empty (AND its inverse: over-fetching when coverage already proves complete); private-event republish / provenance laundering (D10); provenance loss on dedup / re-sign; bypassing insert path or acting on Duplicate/Superseded outcome as canonical; assuming NIP-77 without probe (top sync mistake); manual delete-by-filter from app code.

### Identity / accounts / signer (agent 4)
- **"Sessions are state, switching is an action."** Most-repeated framing (aim.md:248 D7, §4.6). Kernel holds `(accounts vector, active_pubkey)`; each account = signer ref + derived profile/follows/mutes/relay-list views + status. Switch = ONE action; no imperative logout→login→reload.
- **Identity is an INPUT; engine owns DERIVED routing.** (Matches our design goal exactly.) Reads are identity-reactive BY CONSTRUCTION — switching re-derives every account-scoped view with zero app code.
- **Sign/publish orthogonal — structural, not lifecycle.** "Signed ≠ published; published ≠ signing now." Do NOT model as one linear lifecycle (BuildingUnsigned→WaitingForSig→Publishing→Published); keep `awaiting_capability` (generic, covers signing) and `publishing` as INDEPENDENT optional stages. Publish targets resolve via outbox independent of which signer produced the event. Backend-agnostic per account (local/NIP-46/NIP-07/NIP-55).
- **Switch must prove OLD-account TEARDOWN before new-account activation — not additive re-subscribe.** Cross-account leak was a real fixed bug (6ed2df74b/#2976, 834cd868f): stale signer/wallet/relay callbacks silently overwriting the active projection. NWC/Cashu reset per-account on switch. Signer health keyed per-identity so a superseded account's background callback can't leak in.
- **⚠ REAL TENSION (agent 4, not just a gap):** existing `IdentityRuntime` is an ACTOR-OWNED authoritative map (framework-owns-everything) — CONFLICTS with the new "app owns who's active / identity-as-input" framing. Must be explicitly reconciled before MVP claims compliance. The MVP's nsec/multi-nsec login+switch IS the intended proof vehicle for the identity-as-input seam → strengthens the case that **login/identity is the true FIRST slice**, since the timeline query has nothing to route against until it holds.
- **TRAPS:** imperative login/logout/reload (D7); native holding session/derived state (Android's imperative `openTimeline()` post-switch was a flagged violation, removed for view-driven re-derivation); coupling sign+publish; cross-account state leakage.

### Subscriptions / reactive queries (agent 2) — MOST LOAD-BEARING
- **C8/D8: every wire subscription auto-dedup + auto-coalesce + auto-close + auto-buffer.** Dedup = same canonical filter → 1 REQ/relay. Coalesce = compatible filters merge into one broader REQ via a merge lattice, each consumer filtered locally. Auto-close = on last consumer drop or EOSE (one-shot). Buffer ≤60Hz ViewBatch, backpressure→FullState. (framework-magic/subs.md C8, doctrine D8)
- **NO POLLING ANYWHERE EVER** — edge-triggered wakeups only (`kernel.changed_since_emit()`), never volume-proportional; forbidden even in test helpers. (D8, conformance J1)
- **D4: the app NEVER owns the concrete expansion of a derived interest.** "The app never learns or owns the concrete expansion of `$myFollows`."
- **THE $myFollows SPEC = C5 (kind3.md), reconstructed exactly:** trigger = kind:3/NIP-51 people-list lands for active account, replaceable-supersedes → NIP-02 reducer updates an `ActiveUserFollows` `ReducedSource` (the inner live query) → source owner emits an ordered Open/Replace/Refresh/Close delta of `DependentInterestChild` via `apply_dependent_interest_delta` (dependent_interests.rs:173, the authoritative path) → registry/planner (Trellis) diffs to per-relay CLOSE/REQ wire delta. **REPLACE not rebuild** (unchanged authors = zero wire churn); **RECOMPILE not reopen** (outer view handle stays open, refcount unchanged, one updated payload). Teardown = exactly-once close per withdrawn child, reverse-of-open order, one compile-invalidation per batch. Contract test `c5_kind3_change_recompiles_follow_dependent_subs`: {A,B,C}→{A,B,D} yields exactly CLOSE on C's relay + REQ on D's relay, NO churn on A/B.
- **🔴 SMOKING GUN (validates owner's worry):** framework-magic.md footnote 1 admits C5's proof test uses "a synthetic stand-in for the ReducedSource dependent-interest replacement" — the REAL end-to-end reducer path is NOT exercised by the test. So the primitive was carefully spec'd AND partially implemented, but **never truly proven end-to-end.** The MVP's whole job on this axis: drive the REAL reducer path (real kind:3 change → real re-route), not a synthetic push.
- **⚠ merge lattice (filter coalescing) is an OPEN QUESTION** — "lattice formalization not fully formalized"; coalescing correctness asserted by tests, not proven structurally. The "compile filters per relay" MVP requirement rides on an unproven formalization.
- **TRAPS:** enqueue-as-done (status must stream pending→signed→stored→planned→sent); NDK-style teardown+reopen race (events lost in CLOSE→new-EOSE window — delta-patch exists to avoid it); app watching kind:3 directly / re-deriving author lists (the canonical NDK-era bug, forbidden structurally); a FIFTH hand-rolled reconciler instead of collapsing onto shared Trellis/KeyedReconciler (#3115/#3116); poll loops anywhere.

### Boundary / display / diagnostics (agent 5)
- **Top owner-repeated principles (from 240-episode mining):** single-source-of-truth (×34), display-separation (×27: core emits raw tokens only — hex pubkeys, Unix ts, verbatim kind:0; ALL presentation shell-only; `short_npub`/`avatar_initials` legit ONLY in TUI/CLI/fixtures, never projections/snapshots/FFI), operator-policy-at-app-layer (×22: "damus shouldn't be hardcoded ANYWHERE"), generic-layer-purity D0 (×17: "would a second unrelated Nostr app use this unchanged?"), thin-shell (×14: "chirp is supposed to have no fucking logic"), no-polling (×13).
- **Diagnostics ALREADY have durable design** (harvestable): four-lane diagnostic records + reverse-relay-coverage view (subscription-compilation/diagnostics.md) — per-relay `wire_sub_count`, `provenance_count_last_minute`, by-lane counts (NIP-65/hint/provenance/user-configured); `RelayConnectionDiagnostic` rows. Framing: "outbox invisible in ordinary app code but visible in diagnostics and tests"; "distinguish cached facts from coverage facts." Diagnostics are KERNEL-DERIVED actor-owned state, NEVER raw socket/FFI callbacks. → MVP per-relay/per-kind surface = harvest + extend, and it's the **acceptance test made visible.**
- **UI components: owner explicitly PERMITS, hard-gated.** Separate opt-in per-platform layer ON TOP; component-registry precedent (components copied into app trees; "must not import platform runtimes, C ABI/JNI/WASM, or kernel"); "whatever the app developer chooses to ship." v2 status: north-star, not blocking.
- **TRAPS:** presentation in core/projections (#2219: ~29 kernel callsites emitting English toasts); business logic in native (#2220: NIP-05 resolved in Swift); operator relays/seed pubkeys in shared crates; **recreating governance-by-policing** (the redesign's point is to kill the 46-principle-corpus + doctrine-lint enforcement model, replace with types + bug-class ledger).
- **Planning-discipline note:** MVP spec currently lives ONLY in this research note — but per our process design nothing formalizes until v2, so that's acceptable now; graduation point = after first propose/refute round on the API surface.

## Consolidated: what the ledger CHANGES about the MVP
1. **First slice = login/identity + one routed query, NOT timeline-first.** Identity-as-input conflicts with the existing actor-owned `IdentityRuntime`; the timeline query has nothing to route against until the identity seam holds. Login+switch is the proof vehicle for the seam.
2. **The MVP's headline job is to drive the REAL C5 reducer path end-to-end** (the synthetic-stand-in test is the confirmed weak point). `$myFollows` AND `$bookmarkedRelaySets` are two members of ONE dependent-interest family — prove the family, not two features.
3. **Two modes are genuine NEW builds, not harvests:** NIP-51 bookmarked relay sets (kind:30002, ranked by bookmark count) + "relays from follows' 10002s, 2-relay-min, capped." Everything else = harvest+re-justify.
4. **Diagnostic surface must show COVERAGE/watermark state per (filter,relay), not just counts** — coverage is what proves cache-miss authority + negentropy correctness + 2-relay-min + filter-coalescing. It's the acceptance test rendered on screen.
5. **Two structural unknowns to derisk early:** merge-lattice coalescing is not formally proven; identity-as-input vs IdentityRuntime is unreconciled. Both are MVP-critical.

## GENERALIZATION (2026-07-11) — $myFollows is NOT special; the primitive is a reactive filter grammar
Owner: apps must be able to DECLARE their own composite filters, not just consume canned ones. The general capability: "give me events matching filter X where one of the filter's VALUES is itself the result of another filter passed through a projection — reactively, and re-rooted on active-account change with the app doing NOTHING."

**Every filter field value is a Binding:**
- `Literal(set)` — a fixed set.
- `Reactive(IdentityField)` — e.g. `$currentPubkey` = the active account (the reactive ROOT).
- `Derived(inner: Filter, project: Selector)` — the result of running `inner` and projecting a field out of each result event.

**Two owner examples, formalized:**
- `$myFollows` (1 subquery level): outer `kinds:[X], authors:=Derived(inner=kinds:[3],authors:[$currentPubkey], project=Tag(p))`.
- NIP-29 "groups I belong to" (2 subquery levels + identity root): outer `kinds:[39000,39001,39002], #d:=Derived(inner=(kinds:[39002], #p:[$currentPubkey]), project=Tag(d))`. Inner = all 39002 member-lists that p-tag me → project their `d` (group ids) → outer fetches metadata(39000)+admins(39001)+members(39002) for exactly those groups.

**🔴 CORRECTION (#63/#108, 2026-07-12):** the example above is WRONG protocol-wise and was never built this way — kept here only as evidence of an earlier (incorrect) proposal, per this file's own reading rule. NIP-29 kind:39002 is optional, may be access-restricted, and may list only a subset of a group's members — it cannot discover a user's remembered groups/hosts, so no `Derived` over 39002 can recover "groups I belong to." What NMP actually shipped (#108): remembered groups/hosts come from NIP-51 kind:10009 (the Simple groups list), a single-subquery-level demand — `kinds:[10009], authors:=Reactive($currentPubkey)`, the SAME shape as `$myFollows` above, not a nested `Derived` graph. `nmp-nip51` exclusively owns that schema/codec; `nmp-nip29` composes the decoded entries into typed `GroupRef`/`RememberedGroups` values (group id, host relay, optional name). The app then SELECTS one remembered host (app-owned state, never engine-derived) and issues separate host-pinned demands for group discovery (`kinds:[39000]`) and group content (`kinds:[9,30315], #h:[groupId]`), both via the explicit `SourceAuthority::Pinned({host})` scope (#107) — composition ACROSS two independent demands (the remembered-groups list, then a host-scoped read), not one nested projection graph. Establishing CURRENT membership at a known host (as opposed to which groups/hosts a user remembers) is a separate concern NMP has not shipped as of this writing.

**🟢 CORRECTION (#487, 2026-07-15):** kind:10009 public relay favorites are
now editable through the typed NMP action. Add/remove acquires the active
account's source-scoped list, preserves content and all non-matching tags, and
publishes through the exact-base guarded write path. First-list creation is
permitted only after reconciled absence. Remember/forget of individual
`group` entries remains a separate semantic action and is not implied by this
relay-only surface.

**Unification insight:** `$currentPubkey` (a `Reactive` binding used as the inner's `#p` value) and the outer's `Derived` binding are the SAME mechanism — non-literal filter values the engine resolves reactively. Switch active account → `$currentPubkey` changes → inner re-runs for new user → different projected set → outer re-routes. App does nothing. C5's replace-not-rebuild / recompile-not-reopen semantics apply at EVERY node.

**🔴 KEY DESIGN REFINEMENT (mine, to argue):** the "transform function" must be a DECLARATIVE `Selector` from a CLOSED vocabulary (Tag(name), Authors, Ids, Address/a-coord, created_at…), NOT an arbitrary app closure. Reason: the prior-art linchpin — descriptor must be an introspectable/hashable VALUE so the engine can route/dedup/coalesce demand. An opaque closure = Replicache's fatal trap (engine can't derive server demand). The real transforms (extract p-tags, extract d-tags, author, id) ALL fit a small closed structural-projection set → zero expressiveness lost. A use case outside the vocabulary = extend the vocabulary, never allow code.

**🔴 FINDING (stronger vindication):** existing C5 (`dependent_interests.rs`) is a HARDCODED ONE-LEVEL special case — kind:3 → p-tags → authors, welded to follows. The owner's actual primitive is a **recursively-nestable reactive binding graph with declarative projections and an identity root.** So C5 isn't just untested end-to-end — it's a special case masquerading as the general primitive. This is a reactive dataflow/query engine over the event store (nodes=filters, edges=(inner, projection, target-field), roots=literal|identity), evaluated incrementally with per-node CLOSE/REQ deltas. Cost O(graph) per change (echoes Trellis O(total-graph-state)); fine for realistic 2-3 level bounded graphs; needs bounded fan-out (2-relay-min/author caps) at each level.

**Descriptor grammar sketch:**
```
Filter  := { kinds, authors: Binding, tags: {name: Binding}, since/until/limit, ... }
Binding := Literal(set) | Reactive(IdentityField) | Derived(inner: Filter, project: Selector)
Selector:= Authors | Ids | Tag(name) | AddressCoord | ...   // CLOSED, introspectable
IdentityField := ActivePubkey | ...
```

**MVP consequence:** the MVP must prove the GRAMMAR, not two features. Pick two instances of DIFFERENT SHAPE — `$myFollows` (1 level) and NIP-29-groups (2 levels) — so heterogeneity proves it's a general engine, not two hardcoded reads. This is a far stronger falsifier than two similar reads. `$bookmarkedRelaySets` is a third instance (Derived over kind:30002) if we want relay-mode too.

## Promoted frame (2026-07-11)

The exploration converged after the original grammar/identity work. The current
canonical statement is `docs/VISION.md`; detailed ownership lives in:

- `docs/design/query-demand-and-evidence.md`
- `docs/design/durable-write-signing-and-retry.md`
- `docs/design/protocol-modules-and-composition.md`
- `docs/design/bounded-delivery.md`
- `docs/design/ui-components-strategy.md`

### Settled invariants

- NMP is an embeddable sync-and-routing engine, never an app framework. Direct
  Rust and FFI use one invariant-preserving facade; Swift/Kotlin are native
  projections of it.
- The two nouns remain live query and write intent, but a live query's semantic
  demand is `selection + source authority + access context`, not a bare filter.
- The binding graph stays closed and inspectable. Reusable helpers construct
  visible `Derived`/`SetOp` graphs; richer NIP helpers may return typed protocol
  values without creating another demand lifecycle.
- `$currentPubkey` is a reactive/default input, not a global engine identity.
  Changing it reroots only dependent queries. Other account-spanning queries
  remain live.
- Ordinary publication defaults to the signer registered for
  `$currentPubkey`; an explicit identity override is allowed. The selected
  identity is pinned at `Accepted` and cannot drift after an account change.
- Durable `Accepted` means the frozen unsigned body, receipt obligation, and a
  stable canonical pending row are persisted atomically. Missing signer becomes
  `AwaitingSigner(pubkey)`, not failure. The row promotes in place after exact
  signature validation.
- Pending rows reach observers only through the canonical store path.
  Cancellation or terminal pre-signature failure retracts/compensates through
  that path; relay rejection after signing affects the receipt only.
- A destructive replaceable edit may be accepted only against the exact local
  base its protocol operation established. The store compares that base inside
  atomic acceptance and emits a typed conflict without residue; the operation's
  separately declared source evidence authorizes composition but never claims
  global completeness. Native apps cannot mint this guard through raw FFI
  writes and reach it through semantic operations such as NIP-02 follow.
- Durable retry uses logical backoff with one owner per domain and one deadline
  scheduler. There is no hidden durable transport buffer, fixed-rate polling,
  or silent give-up during temporary unavailability.
- NMP persists signing obligations, not raw secrets. Platform SDKs should ship
  standard secure signer providers; apps own identity policy and may replace
  the provider.
- One engine instance is one shared local trust domain. Accounts share the
  canonical cache. AUTH/access context remains attributable acquisition
  evidence, while mutually untrusted users require an explicit destructive
  reset.
- Query snapshots return rows plus compact evidence about current planned
  sources. EOSE and watermarks remain per-source facts; NMP never claims global
  completeness, `syncHealth`, or authoritative emptiness.
- Latest-state query/diagnostic streams may coalesce intermediate frames.
  Durable receipt facts remain reattachable. Every graph, wire, relay, and
  result limit produces exact semantics or explicit shortfall, never silent
  first-N truncation.
- Core is content-agnostic. Opt-in NIP modules own only their exact protocol
  schemas and expose typed builders, queries, and operations. Immutable unsigned
  drafts compose contributions, then core validates and signs once.
- Contextual publication does not transfer kind ownership. NIP-29 may add its
  `h` tag and group-host route to a NIP-68 photo draft while owning only NIP-29
  event schemas.
- Public invariants are the frame; public syntax is provisional. Any shape
  change needs failure evidence, cross-surface impact review, synchronized
  projections/falsifiers, explicit human signoff, and removal of the superseded
  path.

### Earlier conclusions explicitly superseded

- "Current signer" and `$currentPubkey` are not one inseparable global root.
- Account switching does not require a global teardown-before-activate barrier.
  It only diffs demand that actually depends on the changed reactive input.
- A per-relay watermark cannot make an empty cache globally authoritative.
- Kind/schema ownership does not monopolize contextual routing contributions.
- A relay rejection of a signed event does not roll back the canonical row.
- Protocol growth is not a catalog of kind:1-first recipes in core.

The remaining uncertainty is implementation shape and public spelling, not
these ownership boundaries.
