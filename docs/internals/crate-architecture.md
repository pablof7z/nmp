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

The reducer and the async edge left `crates/nmp` as **two crates**:

| crate | owns | must never depend on | status |
|---|---|---|---|
| `nmp-engine` | the deterministic reducer: `EngineCore`, `handle(EngineMsg) → Vec<Effect>` / `tick()`, plus its reducer-coupled satellites (the negentropy FSM it drives turn-by-turn, the publish-queue fact vocabulary, bench hooks) | **`tokio`, `reqwest`, `nmp-nip11`,** the runtime, the facade, any capability crate. Allowed: grammar, store, resolver, router, transport (frame value types), signer, `negentropy`, `nostr` | correct today |
| `nmp-runtime` | the async edge that interprets effects: `EngineThread`, `Handle`, channels/mailboxes, the AUTH driver, sign-event completion, signer registry, pool bridge, the opaque session payload, the NIP-65 assembly glue, NIP-11 service wiring | the facade, any capability crate (its `nmp-nip65`/`nmp-nip11` edges are the two declared protocol edges) | correct today |

`session.rs` went with the runtime rather than staying in the facade:
`EngineThread::spawn` takes a `RestoredSession` and `Handle` owns the live
session state, so the facade only encodes, decodes, and hands one over.

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

**The fence, and how it was checked (#1720).** A manifest claim is only worth
making if someone verified it, and verifying it needs both directions —
a clean dependency list proves nothing if the source reaches an async runtime
some other way, and clean source proves nothing about what the next edit may
add:

- *Manifest:* `crates/nmp-engine/Cargo.toml` names no `tokio`, no
  `crossbeam-channel`, no `futures-channel`, no `reqwest`. Adding one is a
  reviewable line in a file whose whole purpose is that list.
- *Source:* grepping `crates/nmp-engine/src` for
  `tokio|crossbeam|futures_channel|std::thread|std::sync::mpsc|std::net|reqwest`
  returns only the two doc comments that assert the property. Zero real uses.

`crossbeam-channel` also left `crates/nmp`'s manifest. `tokio` stays there for
exactly two facade uses — `Engine::adapter_runtime()`, which hands a protocol
adapter the engine's own runtime handle, and `nip29::records`' await timeout.

Do the same two checks whenever a manifest is the proof. `nmp-nip11`'s
`reqwest`/`httpdate` claim and `nmp-signer`'s zeroize-only claim rest on the
identical pair.

What the cut actually cost, measured rather than estimated: **96 declarations
went from `pub(crate)` to `pub`** — 62 in `nmp-engine` (29 functions, 11
types, 18 struct fields, 4 re-exports), 21 in `nmp-runtime` (14 functions, 3
types, 2 fields, 2 other), and 13 bench-only counters in
`ingest_attribution`. Roughly twice the pre-cut estimate of ~35, and the
excess is concentrated in three places worth naming: the bench counters
(13, all behind `bench-instrumentation`), `session.rs`'s encode/decode pair
and its account mutators (11, unforeseen because `session.rs` moving was not
in the estimate), and struct **fields** rather than methods (20, on five
structs the runtime destructures — `CoreObservationOwnershipCensus`,
`RelayWorkerRequirements`, `RowsSeed`, `RuntimeConfig`,
`PreparedReplaceableMaterialization`; #1721 revisits their shape). Every
widening was chosen by the compiler: the pass only touched declarations
`rustc` reported as private across the new boundary, iterated to a fixed
point, so none was widened by hand or opportunistically.

The third of those is the one that generalises, and it is the rule below's
fourth item arriving from the manifest side. See "what crosses a boundary".

### Check the build shapes that differ, not one invocation

`cargo check --workspace --all-features` is the shape most likely to pass and
the least likely to tell you anything, because feature unification quietly
supplies what a plainer build will not. It has now hidden two master breaks in
two days: a `nip65`-gated `RelayUrl` import (#1687), and — from the engine cut
itself (#1724) — three items gated `#[cfg(any(test, feature =
"bench-instrumentation"))]` that a sibling crate's own `#[cfg(test)]` code
calls.

The second is a class the split created and worth understanding rather than
memorising. Inside one crate, `cfg(test)` covered caller and callee together.
Across a crate boundary it does not: when `nmp-runtime` compiles its lib-test
target, `nmp-engine` is an ordinary dependency built **without** `cfg(test)`,
so a `#[cfg(test)]`-only item on the engine side simply is not there. Anything
one crate's tests need from another must be behind a real *feature* — here
`test-instrumentation`, which the self-dev-dependency pattern turns on.

The answer is not more care. It is running the shapes that actually differ,
which for these three crates is:

```
for p in nmp nmp-engine nmp-runtime; do
  cargo check -p $p --no-default-features
  cargo check -p $p
  cargo check -p $p --all-features
  cargo check -p $p --all-targets --no-default-features
  cargo check -p $p --all-targets
  cargo check -p $p --all-targets --all-features
done
cargo check --workspace{,' --all-targets'}{,' --all-features'}
```

plus `cargo clippy --workspace --all-targets -- -D warnings` **without**
`--all-features` as well as with it — dead-code warnings are shape-dependent
in exactly the same way, and #1724's own first fix produced one.

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

The evidence for that is the census below, specifically its tail: `store` is
touched from six files, `clock` from seven, and neither reaches 50%
concentration; `router` 48%, `resolver` 35%, `attempts` 40%,
`connected_relays` 42%. Those are not fields awaiting an owner. They are the
context every plane runs in, and **no line drawn anywhere puts them on one
side** — which is the same fact as "the reducer is one thing", measured
rather than asserted.

### The field census — picking the next owner by lookup, not judgement

Every `EngineCore` field, with its accesses counted per production file. The
number that decides things is **concentration**: the share of a field's
accesses landing in its top file. Regenerate it rather than trusting this
snapshot — the point is the method, and the counts move:

```zsh
fields=$(awk '/^pub struct EngineCore \{/{f=1;next} f&&/^\}/{exit} f' \
  crates/nmp-engine/src/core/mod.rs \
  | grep -oE '^[[:space:]]+[a-z_][a-z0-9_]*:' | tr -d ' :' | sort -u)
for f in ${(f)fields}; do
  for file in $(find crates/nmp-engine/src -name '*.rs' ! -name '*test*' ! -path '*tests*'); do
    n=$(grep -c "self\.$f\b" $file); [[ $n -gt 0 ]] && echo "$f $n $(basename $file)"
  done
done
```

**114 fields.** 55 sit at ≥70% concentration; the tail below ~50% is the
shared context, and the split between those two groups is the whole finding.

Read it three ways:

1. **≥85% in one file, several fields moving together → owner candidate.**
   `HistorySessions` (#1695) and `RequestAttempts` (#1693) both came out of
   this band. So does live-wire ownership: ten fields at 71–91%, ~75 accesses
   essentially all in `query.rs`, and — the number that actually settles it —
   **exactly one genuinely foreign reader** (`observation.rs`, a membership
   predicate). The other apparently-foreign reads are the bench census
   calling `.len()`, which is not a reader so much as a boundary violation
   the owner is supposed to fix.
2. **100% in one file but standing alone → not a cluster, leave it.** 22
   fields are single-file, and most are counters (`history_rows_examined`,
   `router_compiles`, `diagnostic_snapshots_built`). A lone field is already
   as owned as it can get; wrapping it buys nothing.
3. **Below ~50%, or high concentration with a foreign WRITER → shared
   context.** `store` (49%, six files), `clock` (49%, seven), `router` (48%),
   `resolver` (35%), `attempts` (40%), `connected_relays` (42%),
   `live_wire_requests` (42%), `pending_request_evidence` (30%). These are
   why the reducer stays one crate and one struct: no line drawn anywhere
   puts them on one side.

Two traps this table makes cheap to avoid, both of which cost real time
before it existed:

- **An adjacent name is not an adjacent field.** `live_wire_requests` reads
  like part of live-wire ownership and is not — 42%, spread across three
  files. `author_outbox_wire_owner_counts` reads like it too, and is a bridge
  into the write plane's `author_outbox_route_needs`. Both stay out.
- **High concentration is necessary, not sufficient.** `SessionRegistry`'s
  fields concentrate well (`auth_sessions` 96%, `slot_to_relay` 83%) and it
  still failed: 12 foreign functions, 9 AUTH types in the would-be
  signatures, and `connected_relays` at 42% sitting in the middle of the
  cluster. Concentration finds candidates; foreign *callers* and foreign
  *writers* reject them.

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

`nmp-nip77` was a fourth rejection, on a different failure again: the reducer
holds `Prober` and `Reconciler` as its own fields and matches `NegStep`
directly, so `nmp-engine` would name `nmp-nip77` and `nmp-nip77` would name
`negentropy` — the dependency gains a hop rather than leaving the manifest,
and `negentropy` is on the reducer's allowed list anyway. The contrast with
`nmp-nip11` is the whole lesson: there, the RUNTIME owned the service and the
reducer saw only a value `core` defines itself, which is what let the crate
line remove `reqwest` from the reducer's future manifest. Ask which side owns
the state before asking whether the cluster is self-contained.

### What crosses a boundary

Four places to look, in this order, before concluding anything about a
proposed boundary. The first three were learned by having the coupling hide
in each of them; the fourth by measuring #1720's real cost.

1. **Constructors** — who can make one, and from what.
2. **Teardowns** — `Drop`, `close`, `release_*`; where the reach goes on the
   way out is not where it goes on the way in.
3. **Trait impls** — a blanket impl or a `From` can carry a whole vocabulary
   across.
4. **Public struct fields — both their types and their visibility.**
   - *Types* decide what dependencies come with a value. This is what nearly
     put `reqwest` back inside the reducer in #1716:
     `RelayInformationCapabilityEvidence` carried an
     `Option<RelayInformationError>`, one line inside a struct being moved
     wholesale, and naming it from `core` would have recreated the exact edge
     the cut existed to remove. It became `Option<String>` — the reducer never
     matched a variant — and the whole type dependency went away for a
     `Display` call at the boundary.
   - *Visibility* decides what the cut costs. **What crosses a boundary is
     not only what you call but what you destructure, and both are public
     API.** 20 of #1720's 96 widenings were fields, on five structs the
     runtime destructures; a "count the methods" estimate cannot see them,
     which is how ~35 became 96.

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

The `#[doc(hidden)] pub mod mechanism` door is **deleted**: `nmp-bdd` and
`nmp/tests` name `nmp-engine`/`nmp-runtime` directly instead of reaching
through the product crate's basement. `nmp`'s public API is now exactly its
visible re-export list. What remains before the row above reads "correct
today" is #1707's own work — the per-family `[features]` entries and the
capability modules.

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

**The facade's re-export inventory is a reliable single point of truth for
what is reachable, and the architecture depends on it.** Five reversals have
now run the same census before moving anything — `nmp-media`,
`Row`/materializer, NIP-02, the eight pure re-export doors, NIP-29 — and the
property held every time: **every external consumer reaches these types
through `nmp`'s crate-root re-export, never by naming a lower crate directly
for something `nmp` re-exports.** Every downstream fix across all five was a
pure substitution.

That is what makes capability eviction mechanical instead of a hunt across
the workspace: read the re-export list, and you have read the reachable set.
If the property ever stops holding, these moves stop being cheap — so treat a
consumer that reaches around the facade for a re-exported type as a defect in
that consumer, not a style choice.

**That rule is for production code. Tests proving reducer-internal behaviour
are a different rule, not an exception to this one.** `nmp-nip29`'s own
production files (`group.rs`, `groups.rs`, `group_list_writes.rs`,
`record_observation.rs`, `scope.rs`) reach every engine type through `nmp`
alone, with no `nmp-engine`/`nmp-runtime` edge in the manifest at all. Five
of its test files name `nmp_engine`/`nmp_runtime` directly instead —
`nip29_group_list_headless.rs` drives `nmp_engine::core::{EngineCore,
EngineMsg}` to prove a reducer-level demand shape, the same thing
`nmp/tests` and `nmp-bdd` already do post-cut. That is not a hole in the
facade: `nmp` deliberately does not expose `handle()`, so a test of
reducer-internal behaviour has nowhere else to go. The test is over when it
needs a mechanism door the facade will never open; the production defect is
a consumer that names one anyway when the facade already offers what it
needs.

The one exception is `nmp-ffi`, which also names capability crates directly.
It is not a hole: it does that only for things `nmp` deliberately never
re-exports, because compiled materializers have to be linked into the
staticlib. That is the target shape working. **Do not "fix" it** by routing
`nmp-ffi` through the facade — that would put capability crates back in
`nmp`'s dependency list, which is rule 2 inverted.

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

1. **Protocol-named entries directly under `crates/nmp/src`** — **0**
   (steps 0–4 moved `media/`, `nip02/`, `nip29/`, `nip18.rs`, `nip22.rs`,
   `nip25.rs`, `nip68.rs`, `nipc7.rs`, `blossom.rs`, `asset.rs`,
   `content.rs`; the NIP-65 split then deleted the last one, `nip65.rs` --
   one line of capability convenience, `engine.publish(request.into_
   write_intent())`, wearing an engine-bound signature, not routing
   mechanism). Target **0**, reached. This is #1707's falsifier and the
   primary number: it is rule 2 stated as an `ls`.
2. **Protocol crates named in `crates/nmp/Cargo.toml`** — **0** optional
   dependencies (down from 11; the last, `nmp-nip65`, left with the door
   above). Target **0**, reached.
3. **Lines in `crates/nmp`** — **7,239**, down from 46,898 in this doc's
   prior count and 73,032 when the engine still lived here. Below the
   ≈7,500 target already: the #1720 engine/runtime cut and #1707's own
   capability eviction landed in parallel and compounded rather than
   merely adding. Crude, gameable by moving code, so secondary to (1).
4. **Crates changed by adding a capability** — **one** for direct-Rust apps
   (the capability crate alone; `nmp` needs no companion edit), **two**
   including `nmp-ffi` when a native projection is wanted. Target reached.

**The one exception, counted honestly rather than by either measure
above**: `nmp-runtime`'s own automatic-outbox-discovery routing glue
(`nmp-runtime/src/nip65.rs`, feature-gated) depends on `nmp-nip65` for the
coordinator's neutral vocabulary. That edge is invisible to measures 1–2
because it lives in `nmp-runtime`, not `crates/nmp` -- it is real
regardless. It is not a capability the engine merely executes: it is how
the engine performs its own job of discovering an author's relays for
outbox routing, so it stays. A second production implementor of
author-route discovery is what would change that answer; nothing else
would, and no trait should be built to pre-empt one that does not exist
(the same reasoning that keeps `nmp-store` free of a backend-abstraction
seam, #1495).

### What the crate work did and did not fix

**The crate cut fixed coupling and provability. It did not fix size, and size
was never going to be fixed by moving files between crates.** Anyone reading
measure (3)'s drop from 73,598 to 7,254 and concluding the hard part is done
has read the wrong number.

What actually happened is that ~66,000 lines moved to packages where their
dependencies could be constrained. `nmp-engine` is still ~27,000 production
lines built around one struct that eleven files write to, and no further
crate line addresses that — four candidates were measured and rejected
precisely because a crate boundary is the wrong instrument for it.

What shrinks the reducer is finishing the owner program (#1606: module owners
with private fields, so the compiler finds every violating access, including
in tests, at zero visibility cost) and pushing state-machine transitions onto
the value types — `PendingWrite` owning its own transitions rather than
`write.rs` performing them on it. Neither is a packaging change.

## Decided — not to be relitigated without new facts

- Capabilities live above `nmp`; the absorption direction of #1143/#1563 was
  wrong and is being reversed (#1707, owner ruling 2026-08-15). Steps 0–4
  (media, `Row`/`ReplaceableMaterializer`, NIP-02, the eight bare re-export
  doors, NIP-29) and the NIP-65 split are done; measures 1–2 above read
  zero. NIP-65's routing glue is the one exception, ruled separately and
  kept for the reason above.
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
- **The reducer/runtime cut is two crates, and it is done (#1720).** Open
  question 1 asked what would reverse it — a door still churning fast enough
  that the boundary forces cross-crate rework catching no drift. It did not:
  the cut cost 96 compiler-chosen widenings once, and the fence is verified
  in both the manifest and the source. Reopening needs new facts about
  churn, not a fresh opinion about size.
- **`RelayInformationCapabilityEvidence` is reducer-side** with the
  snapshot→evidence projection in `nmp-runtime` (#1716). Former open
  question 2; feasible as believed, with one correction the attempt found —
  its `last_error` had to become `Option<String>`, because carrying
  `nmp_nip11::RelayInformationError` would have put the HTTP crate back in
  the reducer's imports for a `Display` call the runtime can make itself.

## Open questions

Marked open on purpose; do not infer answers.

1. **FFI scaling for hundreds of capabilities.** Compiled materializers must
   be linked into the native staticlib, so per-capability accumulation
   moves to `nmp-ffi` rather than disappearing. Hand-written per-NIP
   Swift/Kotlin projection does not scale to hundreds of kinds; the likely
   direction is a generic FFI surface (the two nouns + registered-payload
   minting) with typed capability vocabulary in native packages, but nobody
   has designed it. Do not treat today's per-NIP FFI modules as the pattern
   to extend indefinitely.
2. **Where NIP-51 list kinds live.** `nmp-nip29` ships the kind:10009
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
- #1721 — revisit the 20 public struct fields the engine/runtime cut froze.
  Not a defect; a public field is forever in a way a method is not, and five
  structs destructured across a package boundary is a shape worth a second
  look once the dust settles.
