# The target crate architecture

Where the workspace is going: the crate set NMP is converging on, what each
crate owns, which outward seams are settled, and which questions are
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

A crate is first a unit of responsibility and authority. Cargo is one way of
making that boundary structural.

A crate is justified by an independently coherent responsibility: a lifecycle
or state machine, durable authority, independently consumable capability,
external-resource owner, or composition/artifact boundary. Dependency
isolation is one powerful way a crate can enforce that responsibility, but it
is neither necessary nor sufficient. Identical dependency lists do not imply
identical responsibilities. A package may be justified even when both sides
use the same lower-level mechanisms.

Enforcement here is structural, and deliberately does not depend on a checker.
That is a property of the mechanisms below, not a statement about CI — none
of the boundaries below become weaker or stronger for whether CI exists.
Manifest exclusion is one mechanism: a
crate that does not declare a dependency cannot use it. Many important
boundaries are enforced by other structural means — private fields, opaque
types, ownership, state machines, typed messages, constructors, transaction
APIs, consuming APIs, and package boundaries themselves even when the two
sides declare the same dependencies.

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

   **The rule is about event-kind capabilities, and the facade may name a
   protocol MECHANISM. Owner ruling, 2026-08-17 (#1791).** The two are
   different things and the line is already drawn in practice:

   - an **event-kind capability** owns the meaning of some kinds — NIP-02,
     NIP-22, NIP-29, bookmarks. The extension surface for these is real:
     capability N+1 is a new crate, and `nmp` must not name it.
   - a **protocol mechanism** is wire or session machinery every app rides
     whether or not it knows the number — NIP-11 relay information, NIP-42
     AUTH, NIP-77 negentropy. There is no per-app extension surface here;
     `nmp` and `nmp-runtime` need these internally regardless.

   So the facade may re-export NIP-11's values, and an app reaches relay
   information through the facade rather than as a second Cargo line. That is
   not an unexplained exception to rule 2; it is what rule 2 says once
   "capability" is read as the word it actually uses.

   This resolves the contradiction #1791 found: `crates/nmp/src/lib.rs` stated
   "`nmp` must not contain a single line of any capability's meaning" and "an
   app reaches a protocol family through this facade, never as a second Cargo
   line beside it" eighty lines apart, both as principle. Only one could be
   the rule; the first is the rule, and it was never about mechanisms.

   Two things the ruling does **not** settle, and neither is licence to widen
   it. `nmp-nip11` being a non-optional dependency of both `nmp` and
   `nmp-runtime` is still the one inverted edge in the workspace, and #1806
   still owns extracting AUTH, negentropy and relay-info out of the core. A
   permitted re-export is not a permitted dependency direction.

A crate earns its existence by a real reason — a genuine ownership
boundary, an independent lifecycle, an independently consumed artifact, a
platform/build boundary, breaking a cycle, or a dependency that must not
exist — never by size or symmetry (#882, #1627). Splitting anything that
shares mutable state across the seam is worse than leaving it. A missing
forbidden dependency is not a veto.

## The target crate set

Status column: **correct today** = this crate's named job and its outward
seams exist and should not be undone; it is not a claim that the crate
cannot later be decomposed along a responsibility that has not yet been
packaged. **in flight** = decided, being executed under the named issue;
**target** = decided destination, not yet built; **open** = see the
open-questions section.

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
| `nmp-store` | the durable store. Its single redb transaction spans event AND publish-queue tables (`write_ops.rs`, what makes guarantee #9 hold) — the strongest do-not-split in the workspace | transport, router, resolver, engine, protocol crates | correct today |
| `nmp-resolver` | evaluating `Filter` → `ConcreteFilter`s, demand diffing | router (siblings, not layers) | correct today |
| `nmp-router` | demand → per-relay subscription plans (its 3-symbol `nmp-store` edge is deliberate: durable coverage identity) | transport, protocol crates | correct today |
| `nmp-transport` | the generational WebSocket pool | store, router, resolver, protocol crates | correct today |
| `nmp-router-testkit`, `nmp-resolver-testkit` | test fixtures kept out of shipped APIs | — | correct today |

### Protocol vocabulary the engine itself consumes

| crate | owns | notes | status |
|---|---|---|---|
| `nmp-nip11` | NIP-11 relay-information values + the fetch/cache/single-flight service (2,700 lines) | the SOLE `reqwest`/`httpdate` user in the engine's tree — extracting it is what lets the reducer's manifest carry no HTTP client. `RelayInformationCapabilityEvidence` deliberately stayed behind in `core` (reducer input vocabulary) and `runtime/nip11.rs` projects a snapshot into it, so the edge runs `runtime → nmp-nip11` and never the reverse | correct today |

### The routing seam

| crate | owns | notes | status |
|---|---|---|---|
| `nmp-nip65` | kind:10002 relay-list semantics: `RELAY_LIST_KIND`, marker parsing, canonical winner selection, the outbox coordinator, `BootstrapRelayList` | ordinary protocol vocabulary now, engine-free (`nostr` + `nmp-grammar`), on the same shelf as `nmp-nip18` | correct today |
| `nmp-outbox` | the NIP-65 outbox ALGORITHM as an `AuthorRouteProvider` implementation | the unit an application names to choose its routing. Nothing in `nmp`, `nmp-engine` or `nmp-runtime` mentions this crate; a competing algorithm is a third-party crate depending on `nmp-engine`/`nmp-router`/`nmp-grammar` and changing zero lines here | correct today |

`AuthorRouteProvider` (declared in `nmp-engine`, three moments: `reroot`,
`observe_rows`, `observe_evidence`) is the seam. It is **not** an invented
one: the owner named the requirement — "other developers might want to supply
their own NIP-65 outbox routing… routing should be a fairly adapter-friendly
interface" — and the third-party test is stated and passing. The pull side
(`RoutingFacts`) stays engine-owned and concrete, because it is read
synchronously inside the deterministic reducer and foreign code there is
exactly what `nmp-engine`'s manifest exists to forbid.

Construction-time only, and deliberately hostile to becoming more:
`Option<Box<dyn AuthorRouteProvider>>` beside the capability vec at spawn, no
handles, no ids, no registration, no replacement. Two providers would
last-write-win over a whole-fact replacement with no merge rule anyone could
state, so composition is a combinator provider the application writes, refused
as engine policy. Swapping algorithms is spelled: drop the engine, construct
it with the other provider.

### The engine

The reducer and the async edge left `crates/nmp` as **two crates**:

| crate | owns | must never depend on | status |
|---|---|---|---|
| `nmp-engine` | the deterministic reducer: `EngineCore`, `handle(EngineMsg) → Vec<Effect>` / `tick()`, plus its reducer-coupled satellites (the negentropy FSM it drives turn-by-turn, the publish-queue fact vocabulary, bench hooks) | **`tokio`, `reqwest`, `nmp-nip11`,** the runtime, the facade, any capability crate. Allowed: grammar, store, resolver, router, transport (nine value and observation types — see the fence), signer, `negentropy`, `nostr` | runtime seam correct today; internals open |
| `nmp-runtime` | the async edge that interprets effects: `EngineThread`, `Handle`, channels/mailboxes, the AUTH driver, sign-event completion, signer registry, pool bridge, the opaque session payload, NIP-11 service wiring, and driving whichever `AuthorRouteProvider` the application constructed | the facade, any capability crate, **any routing protocol** (`nmp-nip11` is its ONE declared protocol edge; author-route discovery is a contract it drives, not a crate it names) | correct today |

`session.rs` went with the runtime rather than staying in the facade:
`EngineThread::spawn` takes a `RestoredSession` and `Handle` owns the live
session state, so the facade only encodes, decodes, and hands one over.

**The line is DECIDE versus PERFORM.** The engine owns all protocol state and
every invariant over it; the runtime owns zero protocol state and all OS
resources. That is the whole rule, and it is about responsibility, not about
execution style.

Determinism is the **observable** of that responsibility, not the reason for
it. Stating it the other way round — "synchronous therefore engine" — reads
as a rule about code shape and takes you somewhere wrong: `nmp-nip11`'s
value types and parsing are perfectly synchronous, and that framing would
eventually argue them *into* the reducer, which this workspace has already
refused for good reasons. Ask which side owns the state, not which side
blocks.

Why two and not one (measured at `64f14255`):

- The seam is already clean and one-directional: `core → runtime` production
  edges are **zero** (#1684), the runtime drives `EngineCore` through **33
  distinct methods** (75 of 107 call sites are `handle()`), and it touches
  **zero fields** directly. The cut freezes a door that already exists.
- The reason *this* seam is two crates is a dependency constraint only a
  manifest can enforce: `core/` names no `tokio` and no `reqwest` today, and
  nothing but review keeps it that way. Because the reducer owns the protocol
  state, its behaviour must be a pure function of the messages it is handed
  — the foundation of the entire headless falsifier corpus and of
  `architecture-boundaries.md`'s decision/effect split. The manifest makes
  that compiler-enforced: a timer, task, or HTTP call inside the reducer
  stops being a review catch and becomes a build error. This is the
  `nmp-signer`-depends-on-`zeroize`-only play at larger scale, and with zero
  CI it is the only *dependency* enforcement available.

  That argument is why the engine/runtime cut is two crates. It is not the
  general package rule. Query, publication, synchronization, AUTH, session
  state, and other owners inside the deterministic engine are evaluated by
  responsibility, lifecycle coherence, and the facts that must cross.
  Package extraction is allowed when one of those owners develops a stable
  independent interface; a distinct dependency list is not required.
- A corollary that has already had to be applied twice: **a coordinator earns
  ownership of ORDERING between subsystems, not ownership of the subsystems
  themselves.** That is the test every owner extraction in #1606 has been
  passing, stated at package scale — and it is the standing check on
  `nmp-runtime`, whose coordinator file has begun implementing the
  subsystems it is supposed to be sequencing.
- Sequencing: the cut lands after #1707 (capabilities out) and after #1606's
  first owner extractions, so the frozen door is the owner-shaped one, not
  the pre-decomposition one. What could have reversed the two-crate answer,
  and did not, is recorded in the "Decided" section.

**The fence, and how it was checked (#1720).** State what the fence actually
proves, because it is narrower than "the reducer cannot do I/O". The manifest
proves **no *direct* dependency**; it does not prove no *reachable* I/O,
because `nmp-engine` depends on the whole `nmp-transport` crate, whose own
manifest names `mio`, `rustls` and `tungstenite`. A reducer file that called
a blocking `Pool` method would be inside the fence and outside the intent.
That is exactly why the check runs in **both directions, and why neither half
is optional**:

- *Manifest — proves no direct edge.* `crates/nmp-engine/Cargo.toml` names no
  `tokio`, no `crossbeam-channel`, no `futures-channel`, no `reqwest`. Adding
  one is a reviewable line in a file whose whole purpose is that list. This
  is the half that survives the next edit: it constrains what can be added.
- *Source — proves no reachable use today.* Grepping
  `crates/nmp-engine/src` for
  `tokio|crossbeam|futures_channel|std::thread|std::sync::mpsc|std::net|reqwest`
  returns exactly two hits, both doc comments asserting the property
  (`lib.rs:11`, `core/mod.rs:850`). Zero real uses. This is the half that
  covers what the manifest cannot see — reaching I/O through a dependency
  that is already legitimately present.

A clean dependency list proves nothing if the source reaches an async runtime
through an allowed crate, and clean source proves nothing about what the next
edit may add. Neither statement alone is the fence.

The `nmp-transport` edge is also wider than "frame value types" suggests, and
worth naming exactly: the reducer imports **nine** symbols — `RelayFrame`,
`RelayHandle`, `RelayHealth`, `DisconnectReason`, `HandoffResult`,
`AttemptCorrelation`, `CommittedObservationCandidate`,
`CommittedObservationHit`, `CommittedObservationPublication`
(`core/mod.rs:122-126`, `core/auth_core_headless.rs:7`). Several are richer
domain vocabulary than "frame". None is async and none performs I/O, so the
property holds — but the phrase was underselling what the edge pulls in, and
an under-described edge is how the next one gets waved through.

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

## The reducer's decomposition: owners first, packages later

**The destination is `nmp-query` as one crate**, owning the complete
live-query acquisition and projection lifecycle, with `nmp-engine` reduced to
ordering between it and the write plane. That is settled by decision, not by
the measurements below.

The *sequence* to it is not negotiable in the other direction: concrete owners
are built inside `nmp-engine` first, and a package boundary is drawn only once
the interface is known — because Rust's crate boundary turns whatever shape
exists at that moment into permanent public API. Everything below is about
which owners, in what order, and what each extraction proved.

### What the plane-seam measurements do and do not say

Two separate things get run together here, and keeping them apart is the whole
point of this section.

*What is measured.* The read-plane/write-plane seam is wrong, and wrong
structurally rather than by degree: a field-level scan of `EngineCore` shows
**15 fields touched by both the write-plane files and the read-plane files**
(beyond the shared `store`/`clock`/`resolver`/`attribution` context: the
session registry, live request evidence, author-outbox route needs,
negotiation sessions, branch handles), and three named invariants span the
planes — write teardown must emit an observation unsubscribe (the
coordinate-coverage leak guard), store recovery clears write-plane state and
rebuilds the resolver in one sequence, and the exact-generation session
conjunction gates both planes' sends. A write/read crate seam would put
shared mutable state on the wrong side of any line drawn. Four owner-shaped
candidates were then measured one at a time and each failed for its own
reason — `SessionRegistry`, `AttemptCorrelations`, `CoordinateCoverage`, and
`nmp-nip77`, all below.

*What is NOT measured, and must not be inferred from it.* That no package
seam exists. **Four rejected proposals bound the proposals, not the space.**
"These four seams are bad, therefore the reducer is permanently one package"
is a step this document used to take and no longer does; the two owner
extractions that succeeded at zero visibility cost (`RequestAttempts` #1693,
`HistorySessions` #1695) are the standing counter-example, because they
demonstrate the reducer's internals *are* separable — the fact a permanent
ruling talks past rather than answers. `WireOwnership` and `RequestTargets`
(#1746) make it four.

So the standing position is exactly this, and no more:

- The engine/runtime boundary is settled.
- The **write/read plane seam** is measured and wrong. That measurement says
  nothing about the query lifecycle's boundary, and must not be read as if it
  did — an earlier version of this section made exactly that overreach.
- The four examined owner-shaped candidates **do not earn package
  boundaries**, for the reasons measured below, not because they would leave
  both sides depending on the same lower-level crates.
- **#1606 continues through private concrete owners**, and that is now also
  the road to `nmp-query`: each owner hardens one piece of the interface the
  eventual crate boundary will freeze.
- **A crate still requires evidence** of a coherent responsibility with a
  stable independent interface and lifecycle. A distinct dependency list is
  not required. `nmp-query` is expected to meet that bar; the owner work is
  how it gets there, not a substitute for it.

The census below is what the plane-seam bullet rests on, specifically its tail:
`store` is touched from six files, `clock` from seven, and neither reaches
50% concentration; `router` 48%, `resolver` 35%, `attempts` 40%,
`connected_relays` 42%. Those are not fields awaiting an owner; they are the
context every plane runs in, and no line drawn *between the planes* puts
them on one side. Read that as evidence against the plane seam, which is
what it measures — not as evidence against every seam, which it does not.
And read it knowing the instrument's limit: it counts per *file*, and a file
holds both `impl SomeOwner` and `impl EngineCore`, so it charges root
orchestration to whatever owner shares the file. "They all need the same four
things" below is where that is measured properly and comes out differently.

### Owners extracted so far

`RequestAttempts` (#1693), `HistorySessions` (#1695), `WireOwnership` and
`RequestTargets` (#1746), and `Nip77Sessions` (#1747) all came out of the
≥85%-concentration band. `EngineCore` is **114 → 96 fields** across the last
three. What they proved:

**`WireOwnership` — ten fields.** `rebuild_wire_ownership` opened with
**twelve consecutive `.clear()` calls** and then open-coded the owner counting
a second time. Two of those twelve clears belonged to the request-target
owner. The reset is now `WireOwnership::default()` and the replay goes through
the same `retain` the incremental path uses, so a new map cannot be forgotten
by a reset that does not name it.

- The second copy had already **drifted**, which is the finding the reading
  missed: the rebuild skipped `router.activate` and `attribution.observe_atom`
  entirely and rebuilt the author-outbox bridge a different way. They were
  not two spellings of one algorithm awaiting divergence. They had diverged.
- Two refcount rules became one. Per-handle refcounts asserted
  (`checked_sub(..).expect(..)`); the owner counts 200 lines away absorbed the
  same violation with `saturating_sub`. The whole suite passes under the loud
  spelling, so the silent one was protecting no real path.
- Four test files hand-wrote six of the maps to build a 10,000-atom fixture,
  including assigning an owner count directly — a **third** copy of the
  counting, free to invent states the production path cannot reach. Fixtures
  build through the owner's doors now.

**`RequestTargets` — three fields.** Three siblings on the god struct with
nothing saying which was derived from which. They are two layers: `by_handle`
is what a branch *declares*; the other two are what is *live*, derived by
intersecting the declaration with the branch's wire-contributing scopes. The
rebuild's surviving two clears became one named operation,
`forget_activations()` — *forget every activation, keep every declaration* —
and a rebuild that clears one and not the other is no longer expressible.
Activation had also been reaching into `self.handles` to derive which scopes
contribute wire; that is a freshness decision the branch owns, so it now
arrives as a passed-in fact.

**`Nip77Sessions` — eight fields.** The candidate signal here was not
concentration but *repetition*: three clusters with the identical
`(map, reverse-index-by-plan)` shape, each carrying its own hand-written insert
and take. Six functions, verbatim copies, differing only in the value type and
how the plan id was read off it — a fourth cluster would have been copies seven
and eight. One generic `PlanIndexed` replaced them, along with three open-coded
plan-scoped teardowns (`take_plan`) and five "collect the matching ids, then
loop calling take" sites (`take_where`), three of which had discarded the
values in the collect and then looked each one up a second time.

Two removals worth keeping separate from the count. `take_plan` deleted a
`None => {}` arm — a silent no-op if the reverse index named a child the
forward map had lost — by handing back the values, so the match is now
exhaustive over the typed value. And `next_incarnation` became private with no
setter, so "ONLY ever increments" is a property of the file rather than a
request in a doc comment.

**Repetition is a candidate signal in its own right.** The field census finds
clusters by concentration; it cannot see that three *separate* clusters share
one shape. When the same insert/remove dance appears with different types,
count the copies before counting the accesses.

### The falsifier caught the census lying

`rebuild == incremental` had never been asserted, though `recompile` runs the
rebuild over state the incremental path built and nothing marks that state
suspect first. Written as a test, it was green — and **stayed green with the
rebuild's reset deliberately deleted**.

The reason is worth keeping: `CoreOwnershipCensus` counted how many demands
were live but never their owner *counts*. It could not distinguish an owner
count of two from one of four, which means every `assert_eq!(census,
default())` teardown proof in the suite was blind to a wrong-but-nonzero
count. `wire_owner_refs` closes it. Both owners' deliberate breaks are now red
on exactly the field that describes them:

| deliberate break | red field / test |
| --- | --- |
| delete the wire rebuild's reset | `wire_owner_refs` 4 vs 2 |
| clear one of `forget_activations`' two maps | `request_target_refs` 8 vs 4 |
| `PlanIndexed::take` skips the reverse prune | mirror test, after disconnect |
| `PlanIndexed::take_plan` leaves the reverse entry | mirror test, after unsubscribe |

The last two are caught by *different* tests, neither of which catches the
other's break. That is not redundancy missing — it is the two removal
directions having disjoint call sites: the ordinary open/close lifecycle never
routes through single-child `take` at all. An owner with more than one removal
direction needs a falsifier per direction, and finding out which test catches
which is how you learn the call graph you actually have.

This is the shape of the general rule already stated below: a green test is
evidence only once it has been shown to go red for the reason claimed. Here
the break was not detected by the assertion — it was detected by *what the
assertion could see*, and no amount of care in writing the test would have
found that. When adding an owner, add the census observable that would notice
its counts being wrong, not merely absent.

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
   `HistorySessions` (#1695), `RequestAttempts` (#1693) and `WireOwnership`
   all came out of this band. The third is the worked example: ten fields at
   71–91%, ~75 accesses essentially all in `query.rs`, and — the number that
   actually settled it — **exactly one genuinely foreign reader**
   (`observation.rs`, a membership predicate). The other apparently-foreign
   reads were the bench census calling `.len()`, which is not a reader so
   much as a boundary violation the owner is supposed to fix; they became one
   `counts()` call. The prediction held: 59 raw field accesses in `query.rs`
   became 25 named method calls, and the single foreign reader became
   `wire.is_attached(id)`.
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

**Smaller boundaries are not the same as more crates, and they do not enforce
the same thing.** A module and a manifest buy two different guarantees, and
this document used to say they were "identical compiler enforcement —
`E0616` instead of an unresolved import". That is false, and it is the
sentence that made "a module is as good as a crate" sound proven:

- **State encapsulation — what a module buys.** `E0616` fires when code
  outside an item's defining module touches a field not visible to that
  scope. It governs *intra-crate item visibility*. This is real and it is
  what #1606 is collecting: `RequestAttempts`' maps are private, so `write.rs`
  cannot corrupt a reverse index, and the invariant becomes enforceable
  rather than documented.
- **Dependency isolation — what only a manifest buys.** `E0433` fires when a
  path names a crate the manifest does not declare. **Cargo dependencies are
  crate-global: any module may `use` any dependency the package declares, and
  no amount of module privacy narrows that.**

The demonstration is in this crate, not hypothetical. `hex` is declared once
in `crates/nmp-engine/Cargo.toml` and used by two modules that share nothing:
`negentropy/mod.rs` frames NEG-OPEN/NEG-MSG payloads with it, and
`core/write.rs:4956`/`:4988` encodes and decodes durable `explicit-hex:`
routing snapshots with it. The manifest comment names both concerns — and
**that comment is the only thing scoping the dependency.** Module privacy has
no opinion on it: `write.rs` did not have to ask anyone, and if a third
module reaches for `hex` tomorrow the comment simply goes stale with nothing
red. Contrast the package answer: were the NIP-77 FSM its own crate declaring
`hex`, `write.rs` reaching for it would be `E0433` and would have to add its
own reviewable manifest line. `libc` shows the same shape from the other end
— declared for exactly one file's bench counters (`ingest_attribution.rs`),
and reachable from all 57 files in the crate whenever the feature is on.

So a module owner cannot forbid a dependency, and a crate line cannot make a
field private that was already reachable within its module. The two questions
take different evidence, and conflating them is what produced three bad
nominations during #1606:

- *Does it deserve an owner module?* Count foreign versus own accesses to the
  fields. A cluster the rest of the reducer reaches into more than it reaches
  into itself is not a cluster.
- *Does it deserve a crate?* First ask whether it owns a coherent
  responsibility — a lifecycle, exclusive state, and an independently
  falsifiable interface. Then ask whether a package would enforce that
  better than module privacy. Follow the type dependencies and the teardown
  reach; those can reject a premature cut. They cannot, by themselves, prove
  that a coherent owner must stay a module because it still needs
  `nmp-store` or `nmp-router`.

Measured against the second test, the three strongest candidates all failed,
each in a different way, and the failures are the useful part:

- `SessionRegistry` — the rejection stands, but **the reason this document
  used to give was wrong in both directions** (corrected in the cluster table
  below). It was not a headcount: of ~13 foreign functions, 11 are
  `EngineMsg` dispatcher entries in `mod.rs::handle()`, which every owner
  needs and which `RequestAttempts` and `HistorySessions` have too — the
  genuinely foreign, non-dispatch count is **2**, and "9 AUTH types"
  reproduces at **6**. `connected_relays` at 8 foreign versus 6 own is exact,
  and all 8 are reads. The real obstacle is structural and was understated:
  `auth_transport.rs:1435-1437` mutates resolver and store in one call
  (`self.resolver.ingest_observed_detailed(&mut self.store, ..)`) inside a
  function that also reads `slot_to_relay`, and the file emits 29 `Effect::`s
  directly rather than returning outcomes. The transport/auth re-cut that
  would fix the shape is in turn ruled out by the exact-generation session
  conjunction (I3) spanning both planes.
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

> **OVERTURNED by #1806 (Pablo, 2026-08-17).** NIP-77 — with NIP-42 AUTH and
> NIP-11 — must come out of the core. The rejection above is not retracted as
> an *observation*: the dependency really does gain a hop, and "ask which side
> owns the state" is still the right question. What it got wrong is treating
> the answer as a verdict on the boundary. The reducer holding `Prober` and
> matching `NegStep` directly is **the defect the epic names**, not a fact the
> boundary must accommodate — a crate whose stated job is "owns all protocol
> state, touches no socket" links a specific wire protocol's implementation
> (`nmp-engine/Cargo.toml:55`), its generic request-attempt owner is
> six-sevenths one protocol's taxonomy, and four of its own effect types name
> the protocol.
>
> This same section already carries the counter-argument, ~50 lines above:
> *"were the NIP-77 FSM its own crate declaring `hex`, `write.rs` reaching for
> it would be `E0433`."* That is the package answer being preferred on the
> exact axis this rejection dismissed.
>
> The open question is no longer *whether*. It is *what a wire-mechanism seam
> emits, and how the reducer sequences it without naming the protocol*. Until
> that seam is designed, read this section as history, not as guidance.

### "They all need the same four things" — the claim, checked

The argument that carried the most weight above is that every cluster needs
`store`, `clock`, `router` and `resolver`, so a boundary around one prevents
no dependency and buys nothing. As stated it does not hold, for two reasons
that have to be separated before any evidence means anything.

- **It counts unlike things alike.** A type appearing in a signature, a
  read-only fact, a direct mutation, and root orchestration that runs *after*
  an owner returns an outcome are four different relationships. Only some are
  coupling at all.
- **It mostly measures the premise.** Every cluster reaches those four
  through `EngineCore` because every cluster currently *lives on*
  `EngineCore`. That is the thing under investigation, not evidence about it.

It was also unfalsifiable as written. This document used to say "#1606's
conclusion stands: seventeen module owners, zero crates". No enumeration of
seventeen anything existed — not here, not in #1606 (whose body states the
enumeration as future work: owner candidates "must be discovered from a fresh
field-to-transition map"), not in any issue. The number was asserted in #1709
and then repeated as established fact. Same class of defect as the fail-open
gates two sections down: a specific-sounding claim with nothing behind it to
check. The enumeration below is the repair, and it is reproducible — the
grouping is the census's own rule (fields that move together, by top file).

#### The instrument was the problem

The census counts `self.field` per **file**. A file holds two different
things: `impl SomeOwner` and `impl EngineCore`. `history_lifecycle.rs` is the
clean demonstration — `impl HistorySessions` is lines 79–231, `impl
EngineCore` is 233–1747, and **every one** of that file's 21
`self.store`/`self.resolver`/`self.clock` accesses is in the second block.
Counted per file, `HistorySessions` looks like it needs the store and the
resolver. Counted per `impl`, it needs neither. The apparent coupling was
root orchestration, attributed to the owner by the measuring instrument.

Re-run at `impl` granularity across every production file in `nmp-engine`:

| | `self.store`/`clock`/`router`/`resolver` | `Effect::` |
|---|---|---|
| every `impl EngineCore` block | **203** | 138 |
| every other `impl` block | **0** | 11 |

Zero. Not "few". The eleven `Effect::` mentions outside `EngineCore` are six
in a test fixture and five in value types that construct or match one — none
is an owner emitting. **Every access to the shared four in this crate is made
by the root.** That is what the extracted owners promise in prose and what
nothing had checked.

Two honest caveats, because this number is easy to over-read. Only four real
state owners exist today, so "zero in owner impls" is partly a statement
about how little has been extracted. And a coarser function-level measure —
"does a function that mutates this cluster's fields also touch the shared
four?" — over-reports by construction: applied to the *already extracted*
`HistorySessions` it still returns 2 of 7, because it counts the root
functions that drive the owner. Treat that measure as an upper bound, which
is how the table below uses it.

#### The categories

| category | what it means |
|---|---|
| **appears** | the type merely appears in state or a signature — weak coupling |
| **fact** | read-only fact input; passable as an argument |
| **mutates** | direct mutation of store/router/resolver — strong coupling |
| **atomic** | mutation requiring one atomic transaction — potentially decisive |
| **root** | root orchestration *after* the owner returns an outcome — **not owner coupling** |
| **cross** | cross-owner private-state access — boundary failure |
| **package** | independent consumer or build shape — package evidence |

The question asked of each cluster is the sharp one: **after separating
owner-local state transitions from root orchestration, what does the owner
itself require?** Not: what does some `EngineCore` call path that touches
this cluster eventually use.

#### The calibration points — four owners that already exist

| owner | store | clock | router | resolver | evidence |
|---|---|---|---|---|---|
| `RequestAttempts` (#1693) | — | **fact** | — | — | `schedule_retry(.., now: Timestamp)`; `request_attempt.rs:251` — "`now` is an argument rather than a read of `EngineCore::clock`: the owner holds no clock, exactly as it holds no store" |
| `HistorySessions` (#1695) | — | — | — | — | takes no `Timestamp` at all; `history_lifecycle.rs:22` — "no `store`, no `resolver`, no `router`, no `Effect`" |
| `AttributionState` | — | — | — | — | resolver-clean empirically; its contract text omits "resolver" where the other two name all four, which is a comment to fix, not a difference in behaviour |
| `RoutingFactStore` | — | — | — | — | pure fact store behind `RoutingFacts` |

**If the blanket claim were true, none of these could exist in its current
shape.** That is the whole force of the calibration: it is not an argument
that the claim is unlikely, it is four counter-examples already compiled.

#### Two axes, and the argument kept confusing them

Before the verdict, one distinction the original claim collapsed and that
changes what the evidence means.

- **Axis 1 — shared dependence on `store`/`clock`/`router`/`resolver`.** This
  is what "they all need the same four things" was about, and it is what the
  table measures.
- **Axis 2 — cross-owner private-state access.** One cluster reaching
  directly into another's maps (`self.pending`, `self.intent_receipts`,
  `self.wire_owner_counts`) rather than taking the data as an argument; a
  cluster's mutators defined in one file and called from another with no
  struct or trait between them; `AuthorOutboxRouteNeeds` emitting
  `Effect::AuthorRouteNeedsChanged` from its own method instead of returning
  an outcome. This is widespread.

**Axis 2 was being read as evidence for axis 1, and it is not evidence at
all.** That clusters reach into each other does not show they belong
together; it shows the ownership was never modelled. It is a boundary
failure, and it argues **for** #1606's owner work, not against future
boundaries. The four extracted owners do none of it — which is the point of
extracting them.

#### What survives

**The blanket claim does not survive.** Twelve of the fifteen clusters
measured — every one except `PendingWrites`, `LaneProjection`, and
`SessionRegistry`, the three with real store/resolver coupling below — need
only read-only facts and root orchestration; two of them,
`CoordinateCoverage` and `AuthorOutboxRouteNeeds`, touch none of the four in
any mutating function at all. The two rejections this
document leaned on hardest were not about the shared four in the first place
— `CoordinateCoverage` failed on a **cross**-owner teardown call and
`AttemptCorrelations` on the vocabularies its public type drags. "They all
need the same four things" described neither, and described the other ten
wrongly.

Taken one dependency at a time, the four are not one thing and never were:

- **`store` — real, and only on the write plane.** Write-plane clusters
  branch on a store result *within the same function* that mutates their own
  state, not as root orchestration after a return.
- **`clock` — a dependency of nothing.** Category **fact** in every cluster
  on both planes: always a passable `Timestamp` used to compute a deadline,
  never a mid-transition consult. `RequestAttempts` already converted its use
  to an argument and lost nothing.
- **`router` — a dependency of nothing.** No write-plane cluster calls it at
  all. Read-plane use is `plan()`/`admit()`/`admission_incomplete()` fact
  reads in `query.rs`; `router.compile()` has no production call site in the
  reducer.
- **`resolver` — real for two clusters**, not fifteen.

**A much stronger and much narrower claim does survive.** Where the coupling
is real it is real for a nameable reason, and it is always the same reason:
**the cluster's job *is* the shared resource.**

- `PendingWrites` and `LaneProjection` are in-memory mirrors of a durable
  transaction. `nmp-store`'s single redb transaction spanning event and
  publish-queue tables is the strongest do-not-split in the workspace, and
  these two clusters are that transaction's reducer-side half. The
  remove-call-reinsert rollback at `write.rs:4427-4502` is not incidental
  coupling; it is the mirror being kept honest.
- `SessionRegistry` genuinely interleaves — one call mutating resolver and
  store while session fields are in scope.

The audit found a third: one multi-cluster atomic transaction that reopened the
store, rebuilt the resolver, and cleared five other clusters' raw fields in one
function. That was I7, and it was the sharpest single argument against carving
the reducer up. **It has since been deleted outright** — with it the whole
store-reopen, latched-fault and continue-degraded model — so the sharpest
argument against splitting the reducer no longer exists. What replaced it is
weaker in exactly the way that helps: a store operation that fails returns
`PersistenceError` and the caller propagates it, leaving every cluster's state
where it was, so no cross-cluster reset has to be sequenced at all.

One judgment call is left open rather than decided: `AttemptCorrelations`'
minting logic is clean and owner-shaped, but its insert and remove sites sit
inside store-touching, effect-emitting host functions. "The cluster is
entangled" and "the cluster's callers are entangled" are different diagnoses
with different fixes, and this is the second. It does not change the ruling.

So the honest sentence about `SessionRegistry` is not the method count this
document used to give. (For the record those numbers were also wrong: of ~13
foreign functions, **11 are `EngineMsg` dispatcher entries** in
`mod.rs::handle()`, which every owner needs and which
`RequestAttempts`/`HistorySessions` would need too; the genuinely foreign
non-dispatch count is **2**, and the "9 AUTH types" figure reproduces at
**6**. `connected_relays` at 8 foreign versus 6 own is confirmed exactly, and
all 8 are reads.) The honest sentence is: **`SessionRegistry` cannot be
extracted in the shape the other owners use until the frame-ingestion logic
that mutates store and resolver is separated from the pure session
bookkeeping** — a structural obstacle, not a headcount.

**What this changes, and what it does not.** It does not justify one new
extraction, and none is proposed here; see open question 2. What it changes is
what may be *inferred*: "no cluster earns a boundary because they all need the
same four things" was doing work it had not earned, and with it gone, the
reason no crate is justified today is the plain one — **no cluster has yet
been shown to own a coherent responsibility with a stable independent
interface, an independent lifecycle, an independent consumer, an independently
consumed artifact, or a dependency that must not exist.** That is a statement
about evidence not yet produced, which is falsifiable, rather than a claim
about a space known to be empty, which was not. Sharing `nmp-store` or
`nmp-router` with a neighbor is not, by itself, that missing evidence.

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

A crate line can also earn itself when one owner needs a stable independent
interface — a lifecycle, a fact/effect vocabulary, an independently
falsifiable public surface — even if both sides still depend on the same
lower-level crates. Manifest-level proof is the reason reducer-versus-runtime
is a crate line today (no direct `tokio`, no threads, no channels under
`core/` — the cut makes a whole class of impurity a build error) and why
`nmp-nip11` is one (it is the only package in the engine's tree naming
`reqwest`). That is one kind of evidence, not the only kind. **No cluster
inside the reducer has yet been shown to have a stable independent interface
or a dependency that must not exist** — which is a statement about what has
been looked for, not a proof that none does. Producing that evidence for some
cluster is exactly what open question 2 asks for.

### A guard whose failure mode is fail-open is probably untested

> **The corpus proves that correct inputs produce correct outputs. It does not
> prove that the guards which exist to reject stale, hostile, or degraded
> inputs actually reject them.**

Measured, not suspected. Each of `EngineCore`'s eight named invariants was
broken in the reducer — the maintaining line deleted — and
`cargo test -p nmp-engine --all-features` run against the 414-test corpus
(#1727):

| invariant | what it maintains | result |
|---|---|---|
| I1 insertion + removal | publish-queue index mirror | **red** (closed by #1742) |
| I2 | wire owner counts / routing-evidence union | red, **38 failures** |
| **I3** | exact-generation session conjunction | **green — not caught** |
| I4 | history session ↔ handle inversion | red, 2 |
| I5 | request-attempt reverse indexes | red, 2 |
| I6 | coverage/parked asymmetry | red, 1 |
| **I7** | store recovery clears every derived projection | **green — one consequence covered, eleven mechanisms not** *(retired: the machinery is deleted)* |
| **I8** | per-turn reset of the scheduler-suppression flag | **green — not caught** *(retired: the flag is deleted)* |

Re-measured 2026-08-16 against `cargo test -p nmp-engine --all-features`
(423 tests), because the table above and #1742 disagreed. Both halves of I1
are now genuinely caught: deleting `intent_receipts.insert` is 20 failures,
deleting `event_to_receipts.insert` is exactly one —
`a_shared_events_ack_reaches_every_co_owner_after_a_restart`. That is a
well-aimed falsifier: one break, one named failure, no collateral. I3 and I8
reproduced as green exactly as recorded.

I7 and I8 have since stopped existing rather than been closed. NMP's entire
modelling of local store write failure — reopen, latched fault, fault
classification, continue-degraded — was deleted: a store operation that fails
returns an opaque `PersistenceError`, the caller propagates it, and the engine
carries on. There is no recovery sequence for I7 to guard and no suppression
flag for I8 to reset. Six named invariants remain, and the two that were
uncovered by difficulty are now one.

### One falsifier retires one mechanism, not one invariant

I7 is the row worth reading twice, because "not caught" understates it and
"caught" would be false. The mechanism it describes has since been deleted;
the measurement is kept because the rule it produced is what generalises.

#1742 *did* add an I7 falsifier, and it was a good one: it asserted that a
dispatched, unacknowledged write is re-armed from durable keys after a
reopen-required store failure, a real observable consequence of
clear-then-rebuild. But the recovery function maintained I7 with **eleven
separate resets** — one per derived projection — and each was deleted
individually here. **All eleven left the corpus green**, including the clear of
the durable write-obligation map itself. Store recovery could skip rebuilding
it and 423 tests did not notice.

So the ledger row read as covered while eleven independently-deletable lines
sat unguarded. The rule this yields:

> **When an invariant is maintained by more than one line, one falsifier
> retires one line. Count mechanisms, not invariants.**

This is the same defect the census had, one level up, and it appeared three
times in a single day at three different scales: `CoreOwnershipCensus`
counted how many demands were live but not their owner counts;
`plan_edges == child_count` could not see a child indexed under the wrong
plan; and an invariant row named one consequence of eleven mechanisms. Every
one of those assertions was *correct*. Every one was blind to most of what it
claimed. Correctness of an assertion says nothing about its coverage, and the
only way to learn the difference is to break each mechanism separately.

The line is not read-plane versus write-plane. It is:

- **Caught invariants are STRUCTURAL.** Index mirrors and refcounts that
  ordinary traffic reads on the happy path. Break one and the very next
  normal operation misbehaves, so the corpus trips over it without trying.
  I2's 38 failures is what "unavoidable on the normal path" looks like.
- **Uncovered invariants are GUARDS on abnormal paths.** They exist to
  *reject* something — a stale auth epoch, state surviving a store failure,
  a persistent I/O error becoming a `recv_timeout(0)` busy-spin. Every break
  makes the code **more permissive**: I3 drops a conjunct, I8 dropped a reset.
  Nothing fails until a hostile or degraded input arrives, and the corpus
  never constructs one. (I1 was the exception that proved the rule: it is
  structural, but its only readers live one package up, so the reducer's own
  tests did not observe it either — until #1742 wrote a test that does. I7 was
  the exception that survived, and for a different reason: it was not one
  guard but eleven, with a falsifier for one *consequence* of them.) Two of
  these three abnormal-path guards were later deleted along with the whole
  store-failure model they guarded — which is the other way a fail-open guard
  stops being a liability, and the cheaper one.

**This is a habit, not a testing gap, and it is the same one that got an
entire verification apparatus deleted on 2026-08-15.** Every check removed
that day failed identically: an SDK-parity gate that passed against an SDK of
one comment, a source-text scan for `ProbedRelay(` that read one file, two
assertions of the form `x == x`. All green while the thing they protected was
violated. The reducer's guards are the same class one level down — which is
why the rule generalises past the eight named invariants:

**Any guard whose failure mode is fail-open is probably untested.** See
#1727, and #1736 for the fixture the corpus needs before I3 can be
falsified at all. I7 and I8 needed no fixture and got no falsifier; the
store-failure model they guarded was deleted instead.

### A red falsifier is only evidence if it is red for the right reason

The sibling of the rule above, and the same family of lie. Fail-open says a
guard can be **green while violated**. This says a falsifier can be **red
while proving nothing**.

Break-then-restore proves a test *responds* to the change. It does not prove
it responds *to that change*. Those are different claims, and the gap between
them is where a test quietly becomes decorative: it goes red the day you
break the invariant, and then goes red forever after for an unrelated setup
failure nobody looks at.

The failure is easy to walk into because the two causes are indistinguishable
from the assertion's side. I1's falsifier asserts that a semantic
generation's `WriteFact::Destinations` reaches every member receipt. Delete
the `event_to_receipts` half and the fan-out degrades to the owner alone —
red. But *the generation never forming* also produces one receipt, and also
goes red. Same assertion, same colour, completely different meaning.

**The discipline is one line: assert the precondition holds BEFORE the break
makes the property observable.** Establish that the multi-member generation
exists, and only then assert the fan-out. That converts "this test went red"
into "this test went red because the invariant broke".

Four instances of a test lying about what it knows have surfaced in two days
— three fail-open gates and one falsifier that passed with its subject fully
reverted, because a helper swallowed the effect being asserted (#1683). Both
rules exist because neither is obvious while you are writing the test that
has the problem.

### Which package does a test belong to

**"Drives `EngineCore`" does not make a test a reducer test. What decides it
is which crate's vocabulary the test insists on.**

The corpus move (#1728) put 157 headless tests into `nmp-engine` and two
files refused, each in an instructive direction:

- `nip29_group_reads.rs` mints every demand through
  `nmp_nip29::group_demand_at`, and its own header says the point is that
  nothing in it re-implements the door. `nmp-nip29` sits ABOVE `nmp-engine`,
  so a test that insists on the real constructors cannot live below them.
  It is a **capability falsifier that happens to drive the reducer**, and it
  moved UP, to `crates/nmp-nip29/tests/`.
- `write_scheduling.rs` imported `nmp_runtime::FACT_CHANNEL_CAPACITY` — a
  reducer test naming the runtime. The reducer's contract is "no page exceeds
  the bound you handed me", so the number is the caller's business and the
  test is the caller: it became a local constant, and the test moved DOWN.

Both were invisible while everything lived in one package. That is the split
paying out in a way nobody predicted, and it is why the question is worth
asking per file rather than per directory.

Two fixes to reject when a test does not fit, both considered and refused
for `nip29_group_reads.rs`:

- **A cross-package `#[path]` include** reaching into another crate's
  `tests/`. It compiles. It also makes one package's test layout load-bearing
  for another's, invisibly.
- **A dev-dependency running UP**, from the reducer to a capability crate.
  Cargo permits the cycle, which is exactly the trap: it would make
  `cargo test -p nmp-engine` build the entire facade and capability stack,
  and it inverts the direction the split exists to establish. A dev-dependency
  is still a dependency in the file that is supposed to be the proof.

Copying eight short fixture helpers was cheaper than either.

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

**The exception is gone.** `nmp-runtime` used to own automatic-outbox-discovery
glue (`nmp-runtime/src/nip65.rs`, feature-gated) that depended on `nmp-nip65`,
counted honestly here because measures 1–2 could not see it. It is deleted: the
routing seam above replaced it, `nmp-runtime` names no routing protocol, and
the `nip65` cargo feature is gone from both `nmp-runtime` and `nmp` (taking
the cfg-shape break class of #1687 with it). What changed the answer is what
this section said would change it — not a second implementor discovered in the
wild, but the owner stating that supplying one is a product requirement. That
is the difference between a seam invented to pre-empt a hypothetical (#1495's
`EventStore`) and a seam with a named consumer and a stated acceptance test.

### What the crate work did and did not fix

**The crate cut fixed coupling and provability. It did not fix size, and size
was never going to be fixed by moving files between crates.** Anyone reading
measure (3)'s drop from 73,598 to 7,254 and concluding the hard part is done
has read the wrong number.

What actually happened is that ~66,000 lines moved to packages where their
dependencies could be constrained. `nmp-engine` is still ~27,000 production
lines built around one struct that eleven files write to, and no crate line
yet *proposed* addresses that — four candidates were measured and each
rejected. That bounds the proposals, not the space, and it is not a reason to
wait: shrinking the reducer was never going to be a packaging change anyway.

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
  zero. NIP-65's routing glue was the one exception and is now closed too:
  it is `nmp-outbox`, an `AuthorRouteProvider` an application constructs.
- **There is no `nmp-nip51` crate, and there never will be. Owner ruling,
  2026-08-17:** *"no, nip-51 crate should likely not exist ever."* This
  document previously deferred the question, to be answered when a second
  NIP-51 family arrived; bookmarks is that family, so it was about to be
  answered by default the moment anyone built its door.

  The rule the ruling states: **NIP-51 is a numbering document, not a
  domain.** A crate owns the kinds whose *meaning* it owns, never the kinds
  one spec happened to register in the same file. Bookmarks (kind:10003)
  live in `nmp-bookmarks`; the simple-groups list (kind:10009) lives in
  `nmp-nip29`, because a saved-groups list is only meaningful to something
  that understands groups. The owner confirmed that split directly —
  *"this is correct!"*

  An `nmp-nip51` crate would be a category Nostr does not have, holding
  unrelated kinds together for an editorial reason rather than a semantic
  one — the same defect as any other invented category
  (`conventions/naming-no-invented-categories.md`), applied to crate
  boundaries instead of type names. **Do not "fix" the absence**, and do not
  let an architecture diagram draw a single NIP-51 box; the absence reads
  like an omission to anyone scanning the crate list, and someone will
  eventually try to close it.

- `Row`/`RowSignature` and the `ReplaceableMaterializer` contract belong in
  `nmp-grammar` (#1707 steps 1–2); the contract's own imports prove it needs
  nothing from the engine.
- Capability registration is construction-time, compiled, and closed —
  `Engine::new_with_capabilities(config, Vec<ReplaceableMaterializerSpec>)`,
  duplicate and unknown program/format refused before custody (#1624). No
  runtime registration, no registry object, no capability super-trait.
- **Routing installation is the same shape**, and for the same reason:
  `Engine::new_with_capabilities_and_routing(config, capabilities,
  Option<Box<dyn AuthorRouteProvider>>)`. One provider, chosen at
  construction, fixed for the engine's life. No registration, no replacement
  generations, no unregister — #1624 deleted exactly that machinery for
  capabilities and the routing API must give it nothing to grip.
- The load-bearing boundaries: `nmp-store`'s single write transaction;
  `nmp-signer`'s zeroize-only manifest; `nmp-grammar`'s dependency-light hub
  position; the manifest-is-the-proof crates; `nmp-cli`'s zero NMP deps;
  and the fact that no crate below the facade names a routing protocol.
- No backend abstraction over the store without a real second backend
  (#1495), and no invented seams: a trait with one production implementor
  and no cycle to break is dead surface.
- **The reducer/runtime cut is two crates, and it is done (#1720).** A
  former open question asked what would reverse it — a door still churning
  fast enough that the boundary forces cross-crate rework catching no drift.
  It did not: the cut cost 96 compiler-chosen widenings once, and the fence
  is verified in both the manifest and the source. Reopening needs new facts
  about churn, not a fresh opinion about size.
  **This settles one boundary, not every boundary.** The evidence here is
  about the line between `nmp-engine` and `nmp-runtime`; it says nothing
  about lines *inside* `nmp-engine`, which are open question 2.
- **`RelayInformationCapabilityEvidence` is reducer-side** with the
  snapshot→evidence projection in `nmp-runtime` (#1716). A since-closed open
  question; feasible as believed, with one correction the attempt found —
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
   has designed it.
2. **What constitutes the deterministic engine, and whether any owner
   inside it earns a package.**
   The engine/runtime boundary is settled. The internal package
   decomposition is not. Today's packaging is one crate because the measured
   seams (the plane split and the four examined owner-shaped candidates) did
   not earn a package on *their* evidence. That bounds those proposals, not
   the space.

   #1627 already names this distinction: query/publication lifecycle owners
   may become packages while related protocol vocabularies with identical
   dependency/artifact shapes remain feature-gated modules.

   #1606 continues through private concrete owners regardless. A package
   extraction is allowed when one of those owners develops a stable
   independent interface; it is not an invitation to split by size,
   symmetry, or a target crate count (#1627).

## Related epics

- #1707 — capability eviction (rule 2 executed; steps 0–4).
- #1606 — `EngineCore` internal owner decomposition (module owners first;
  packages when an owner has a stable independent interface; sequenced
  before the engine cut froze the runtime door).
- #882 — package and lifecycle ownership (the crate-reason criteria this
  document applies).
- #1627 — records the boundary criteria and rejects metric-driven
  decomposition; this document is bound by it.
- #1745 — corrects the manifest-first framing that made agents treat a
  distinct dependency list as a crate prerequisite.
- #1739 — the correction that produced the cluster table, the two-guarantees
  wording, and open question 2: this document had ruled out a future reducer
  crate on evidence that only rejected four specific seams.
- #1721 — revisit the 20 public struct fields the engine/runtime cut froze.
  Not a defect; a public field is forever in a way a method is not, and five
  structs destructured across a package boundary is a shape worth a second
  look once the dust settles.
