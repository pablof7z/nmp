# The target crate architecture

Where the workspace is going: the crate set NMP is converging on, what each
crate owns, which boundaries are already correct, and which questions are
genuinely open. `AGENTS.md` points here from the cold-start reading order.

This document exists because of a specific failure mode, observed 2026-08-15:
with no stated destination in the repository, changes were checked against
GitHub issues that encoded older decisions, and a sequence of locally
well-reasoned steps moved the architecture backwards — capability code was
absorbed into `nmp` (#1143, #1563) and a correct crate was deleted, each step
citing a real prior decision. Issues are temporary work artifacts; the
destination has to live here, in the document that owns the subject.

**What this is not.** Not a review checklist, not an approval step, not a
gate. Nobody satisfies this document before merging; you read it to know
which way is forward. Where it marks a question open, the honest state is
"open" — an agent that needs the answer designs it and updates this document
in the same change, rather than inferring a decision that was never made.

Enforcement is structural only. This repository has zero CI by decision, so
nothing below depends on a checker: every boundary is enforced by what a
crate's manifest can and cannot declare, and by module privacy. A crate that
does not declare a dependency cannot use it.

## The two rules that generate the shape

1. **`nmp` is a thin integration layer over focused primitive crates.** It
   owns assembly (config → construction) and the one supported public
   surface (a live query, a write intent, diagnostics). It is not the
   engine and it is not the capabilities. Its purpose is to REDUCE surface.

2. **`nmp` knows nothing about any specific event-kind capability.** Not
   NIP-02, not NIP-29, not bookmarks, none of the hundreds of replaceable
   kinds NMP will support. A capability owns its meaning in its own crate,
   composes ordinary `nmp-grammar` values, and is supplied to the engine at
   construction as a compiled `ReplaceableMaterializerSpec`. Stated as the
   testable property: **adding capability N+1 changes zero lines of
   `crates/nmp`** — and the compiler enforces it, because `nmp` has no
   dependency edge to any capability crate. (#1707 is the eviction epic.)

A crate earns its existence by a real reason — independent dependency
constraints, an independently consumed artifact, a genuine ownership
boundary, breaking a cycle, an independent lifecycle, or a platform/build
boundary — never by size or symmetry (#882, #1627). Splitting anything that
shares mutable state across the seam is worse than leaving it.

## The target crate set

Status column: **correct today** = exists and must not be "improved";
**in flight** = decided, being executed under the named issue; **target** =
decided destination, not yet built; **open** = see the open-questions
section.

### Contracts and neutral values

| crate | owns | must never depend on | status |
|---|---|---|---|
| `nmp-grammar` | the value vocabulary both sides speak: `Filter`/`Binding`/`Demand`/`LiveQuery`, `WriteIntent`/`WritePayload`/`ReplaceableOperation`, tagging, NIP-19 codec — plus (in flight, #1707 steps 1–2) `Row`/`RowSignature` and the `ReplaceableMaterializer` contract | any other NMP crate. Deps stay `nostr` + `blake3` + `serde_json`. 16 consumers; the hub | correct today + in flight additions |
| `nmp-signer` | the signing contract | anything but `zeroize` — that manifest is what keeps the contract crypto-agnostic | correct today |
| `nmp-local-signer` | the one in-process key provider | — | correct today |
| `nmp-asset` | exact-byte asset identity (`Sha256Hash`, `VerifiedAsset`) | any protocol or NMP crate; `sha2` only | correct today |

### Mechanism primitives

| crate | owns | must never depend on | status |
|---|---|---|---|
| `nmp-store` | the durable store. Its single redb transaction spans event AND publish-queue tables (`write_ops.rs`, bug-class #9's falsifier) — the strongest do-not-split in the workspace | transport, router, resolver, engine, protocol crates | correct today |
| `nmp-resolver` | evaluating `Filter` → `ConcreteFilter`s, demand diffing | router (siblings, not layers) | correct today |
| `nmp-router` | demand → per-relay subscription plans (its 3-symbol `nmp-store` edge is deliberate: durable coverage identity) | transport, protocol crates | correct today |
| `nmp-transport` | the generational WebSocket pool | store, router, resolver, protocol crates | correct today |
| `nmp-router-testkit`, `nmp-resolver-testkit` | test fixtures kept out of shipped APIs | — | correct today |

### Protocol vocabulary the engine itself consumes

| crate | owns | notes | status |
|---|---|---|---|
| `nmp-nip65` | kind:10002 relay-list semantics + the outbox coordinator | THE one declared protocol edge below the engine (feature-gated). Deliberately not trait-abstracted: one implementor, no cycle — a seam there would be the `EventStore` shape (#1495) | correct today |
| `nmp-nip11` | NIP-11 relay-information values + the fetch/cache/single-flight service (2,700 lines) | the SOLE `reqwest`/`httpdate` user in the engine's tree — extracting it is what lets the reducer's manifest carry no HTTP client. `RelayInformationCapabilityEvidence` deliberately stayed behind in `core` (reducer input vocabulary) and `runtime/nip11.rs` projects a snapshot into it, so the edge runs `runtime → nmp-nip11` and never the reverse | correct today |

### The engine

Today `core/` (42,043 lines) + `runtime/` (13,956) + satellites live inside
`crates/nmp`. Target: they leave `nmp` entirely, as **two crates**:

| crate | owns | must never depend on | status |
|---|---|---|---|
| `nmp-engine` | the deterministic reducer: `EngineCore`, `handle(EngineMsg) → Vec<Effect>` / `tick()`, plus its reducer-coupled satellites (the negentropy FSM it drives turn-by-turn, the publish-queue fact vocabulary, bench hooks) | **`tokio`, `reqwest`, `nmp-nip11`,** the runtime, the facade, any capability crate. Allowed: grammar, store, resolver, router, transport (frame value types), signer, `negentropy`, `nostr` | target |
| `nmp-runtime` | the async edge that interprets effects: `EngineThread`, `Handle`, channels/mailboxes, the AUTH driver, sign-event completion, signer registry, pool bridge, the NIP-65 assembly glue, NIP-11 service wiring | the facade, any capability crate (its `nmp-nip65`/`nmp-nip11` edges are the two declared protocol edges) | target |

Why two and not one (measured at `64f14255`):

- The seam is already clean and one-directional: `core → runtime` production
  edges are **zero** (#1684), the runtime drives `EngineCore` through **33
  distinct methods** (75 of 107 call sites are `handle()`), and it touches
  **zero fields** directly. The cut freezes a door that already exists.
- The reason is a dependency constraint only a manifest can enforce: `core/`
  names no `tokio` and no `reqwest` today, and nothing but review keeps it
  that way. The reducer's determinism — the foundation of the entire
  headless falsifier corpus and of `architecture-boundaries.md`'s
  decision/effect split — becomes compiler-enforced: a timer, task, or HTTP
  call inside the reducer stops being a review catch and becomes a build
  error. This is the `nmp-signer`-depends-on-`zeroize`-only play at larger
  scale, and with zero CI it is the only enforcement available.
- Sequencing: the cut lands after #1707 (capabilities out) and after #1606's
  first owner extractions, so the frozen door is the owner-shaped one, not
  the pre-decomposition one. See open question 1 for what could still
  reverse the two-crate answer.

**The reducer stays one crate.** This is a defended "one thing, merely
large", not a default: a field-level scan of `EngineCore` shows **15 fields
touched by both the write-plane files and the read-plane files** (beyond the
shared `store`/`clock`/`resolver`/`attribution` context: the session
registry, live request evidence, author-outbox route needs, negotiation
sessions, branch handles), and three named invariants span the planes —
write teardown must emit an observation unsubscribe (the coordinate-coverage
leak guard), store recovery clears write-plane state and rebuilds the
resolver in one sequence, and the exact-generation session conjunction gates
both planes' sends. A write/read crate seam would put shared mutable state
on the wrong side of any line drawn. The reducer's internal decomposition is
#1606's module-owner work, and #1606's conclusion stands: seventeen module
owners, zero crates — none of the clusters has an independent dependency,
consumer, or lifecycle, so crate-ing one would only convert `pub(super)`
into permanent public API.

**Smaller boundaries are not the same as more crates.** Rust module privacy
gives the identical compiler enforcement — `E0616` instead of an unresolved
import — without freezing a churning internal surface as public API. So the
two questions take different evidence, and conflating them is what produced
three bad nominations during #1606:

- *Does it deserve an owner module?* Count foreign versus own accesses to the
  fields. A cluster the rest of the reducer reaches into more than it reaches
  into itself is not a cluster.
- *Does it deserve a crate?* Follow the type dependencies and the teardown
  reach. Zero foreign readers is necessary and nowhere near sufficient.

Measured against the second test, the three strongest candidates all failed,
each in a different way, and the failures are the useful part:

- `SessionRegistry` — 12 foreign functions and ~15 public methods before the
  first line is written, 9 AUTH types in the signatures, `connected_relays`
  at 8 foreign versus 6 own accesses. The transport/auth re-cut that would
  fix the shape is ruled out by the exact-generation session conjunction (I3)
  spanning both planes.
- `AttemptCorrelations` — 2 fields, 6 accesses, all in one file, and still
  no: `AttemptCorrelationTarget` names `ReceiptId` (core-owned),
  `RelaySessionKey` (`nmp-grammar`) and `PublishQueueLaneKey` (`nmp-store`).
  A ~50-line crate whose public type drags three vocabularies is a
  dependency edge bought for nothing.
- `CoordinateCoverage` — `release_coordinate_coverage` calls
  `self.on_unsubscribe(..)`. Teardown reaches into the query plane, which is
  I6 stated as a call graph.

A crate line earns itself when the boundary needs *manifest*-level proof: a
dependency that must not exist. That is exactly why reducer-versus-runtime is
a real crate line (zero `tokio`, zero threads, zero channels under `core/` —
the cut makes determinism a build error) and why `nmp-nip11` is one (it is
the only package in the engine's tree naming `reqwest`). Neither property is
available to any cluster inside the reducer.

### The facade

| crate | owns | must never depend on | status |
|---|---|---|---|
| `nmp` | `Engine` (lifecycle gate, construction incl. the capability vec), `EngineConfig`, `EngineError`, `Subscription`/`Frame`/`Window`, the diagnostics snapshot family, session/auth-policy surface, and the re-export list that IS the public API | **any capability crate, any protocol crate.** Deps: `nmp-runtime`, `nmp-grammar`, `nostr` | target (today it violates rule 2; #1707 is the fix) |

When the target is reached, `crates/nmp` is roughly 4,000 production lines,
its `[features]` table loses every per-family entry, and the
`#[doc(hidden)] pub mod mechanism` door is deleted — harnesses name
`nmp-engine`/`nmp-runtime` directly instead of reaching through the product
crate's basement.

### Capabilities — above the facade, one crate per family

Each capability crate owns ONE family's meaning: kinds, row/tag semantics,
write composition (via the grammar-level materializer contract for
replaceable kinds), and — where the family has a live read shape — snapshot
folding over the generic public surface. Verified: every symbol today's
NIP-02/NIP-29 doors touch is already `pub` on the facade, so the doors move
above it without one new public item.

| crate | engine-bound? | status |
|---|---|---|
| `nmp-nip02` (kind:3 + follow door) | yes — depends on `nmp` | in flight (#1707 step 3) |
| `nmp-nip29` (groups + `RelayScope` door) | yes — depends on `nmp` | in flight (#1707 step 4) |
| `nmp-nip18`, `nmp-nip22` (→ `nmp-nip73`), `nmp-nip25`, `nmp-nipc7`, `nmp-content`, `nmp-nip68`, `nmp-blossom`, `nmp-media` | no — engine-free composition | correct today |
| future families (bookmarks, mute lists, …) | as needed | one crate each, when the behavior arrives |

The direction of the capability edge is the load-bearing fact:
`capability → nmp`, never `nmp → capability`. That single direction is what
makes rule 2 a property of the dependency graph instead of a policy.
Do NOT merge the small capability crates into a bundle (#1562 is declined
under this architecture): the crate a capability lives in is the unit an
application names, and a bundle recreates the accumulation problem one
level down.

### Platform artifacts and harnesses — all correct today

`nmp-ffi` (the one staticlib; it legitimately names capability crates per
feature, because compiled materializers must be linked — it is an assembly,
not the product core), `nmp-cli` (capability selection; zero NMP deps — a
build tool must not link what it builds), `nmp-bdd`, `nmp-parity`,
`nmp-test-support`, `nmp-consumer-check`, `nmp-nip65-consumer-check`,
`tools/*`, the detached `benchmarks/*` workspaces. The manifest-is-the-proof
crates must not be folded into tests; the proof is the manifest.

## The measure

Mechanical, no checker required:

1. **Protocol-named entries directly under `crates/nmp/src`** — today
   **11** (`nip02/`, `nip29/`, `nip18.rs`, `nip22.rs`, `nip25.rs`,
   `nip65.rs`, `nip68.rs`, `nipc7.rs`, `blossom.rs`, `asset.rs`,
   `content.rs`; `media/` already gone, #1708). Target **0**. This is
   #1707's falsifier and the primary number: it is rule 2 stated as an `ls`.
2. **Protocol crates named in `crates/nmp/Cargo.toml`** — today **11**
   optional dependencies. Target **0**.
3. **Lines in `crates/nmp`** — today **73,032**. Target ≈ **7,500**
   (~4,000 production + facade tests). Crude, gameable by moving code, so
   secondary to (1).
4. **Crates changed by adding a capability** — today three (`nmp`,
   `nmp-ffi`, the capability crate). Target **one** for direct-Rust apps
   (+`nmp-ffi` only when a native projection is wanted).

## Decided — not to be relitigated without new facts

- Capabilities live above `nmp`; the absorption direction of #1143/#1563 was
  wrong and is being reversed (#1707, owner ruling 2026-08-15).
- `Row`/`RowSignature` and the `ReplaceableMaterializer` contract belong in
  `nmp-grammar` (#1707 steps 1–2); the contract's own imports prove it needs
  nothing from the engine.
- Capability registration is construction-time, compiled, and closed —
  `Engine::new_with_capabilities(config, Vec<ReplaceableMaterializerSpec>)`,
  duplicate and unknown program/format refused before custody (#1624). No
  runtime registration, no registry object, no capability super-trait.
- The load-bearing boundaries: `nmp-store`'s single write transaction;
  `nmp-signer`'s zeroize-only manifest; `nmp-grammar`'s dependency-light hub
  position; the manifest-is-the-proof crates; `nmp-cli`'s zero NMP deps;
  the one-way `nmp(-runtime) → nmp-nip65` protocol edge.
- No backend abstraction over the store without a real second backend
  (#1495), and no invented seams: a trait with one production implementor
  and no cycle to break is dead surface.

## Open questions

Marked open on purpose; do not infer answers.

1. **Reducer/runtime as two crates: final confirmation and timing.** The
   two-crate target above is this design's answer, argued from the zero
   back-edge, the 33-method door, and the tokio-absence manifest constraint.
   What would reverse it into a single `nmp-engine` crate: evidence at cut
   time that the door is still churning fast enough that the crate boundary
   forces weekly cross-crate API rework with no drift actually being caught
   — i.e. re-run the door census (distinct `EngineCore` methods called from
   `runtime/`) after #1606 steps 1–3 land; if it has grown materially past
   ~33, cut one crate first and split second. Either way the cut follows
   #1707 and the first #1606 owners; it never precedes them.
2. **Where `RelayInformationCapabilityEvidence` lands.** The reducer
   consumes it (`core/mod.rs`); the `nmp-nip11` service produces it. For
   the reducer's manifest to stay `reqwest`-free, the evidence type must be
   defined reducer-side with the snapshot→evidence conversion in
   `nmp-runtime`. Believed feasible (the type is plain data); unverified
   until the extraction is attempted.
3. **FFI scaling for hundreds of capabilities.** Compiled materializers must
   be linked into the native staticlib, so per-capability accumulation
   moves to `nmp-ffi` rather than disappearing. Hand-written per-NIP
   Swift/Kotlin projection does not scale to hundreds of kinds; the likely
   direction is a generic FFI surface (the two nouns + registered-payload
   minting) with typed capability vocabulary in native packages, but nobody
   has designed it. Do not treat today's per-NIP FFI modules as the pattern
   to extend indefinitely.
4. **Where NIP-51 list kinds live.** `nmp-nip29` ships the kind:10009
   simple-groups list (NIP-51's schema) as a documented product-capability
   decision. When a second NIP-51 family arrives (bookmarks), decide
   whether an `nmp-nip51` crate owns the list schemas — do not decide it
   by precedent-matching either way.

## Related epics

- #1707 — capability eviction (rule 2 executed; steps 0–4).
- #1606 — `EngineCore` internal owner decomposition (module owners, zero
  new crates; sequenced before the engine cut freezes the door).
- #882 — package and lifecycle ownership (the crate-reason criteria this
  document applies).
- #1627 — records the boundary criteria and rejects metric-driven
  decomposition; this document is bound by it.
