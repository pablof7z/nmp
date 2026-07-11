# Scoped acquisition evidence — #49 / #12 / #8 (evidence half)

- **Status:** IMPLEMENTATION PLAN (Opus designer). No code written. #43 step-5 frame.
- **Scope:** Replace the engine-global `QueryCoverage::CompleteUpTo | Unknown`
  query-result value with **rows + compact, per-current-plan acquisition
  evidence**; fix derived-query coverage to account for interior atoms (#12);
  reserve the AUTH phase in the per-source evidence vocabulary (#8 evidence half).
- **Nature: this is a REWORK, not a greenfield add.** The coverage-watermark
  substrate (`nmp-store::coverage`, `attribution.rs`) already exists and is
  correctly scoped; the collapse into a global claim lives in exactly one place
  (`coverage_query.rs`) plus its FFI projection. That is what this frame deletes.
- **Governs a public surface change** → rides the #52 governance protocol
  (surface-change-log entry + snapshot regen + synchronized falsifiers + parity).

Authoritative contract (from #43 / #49 / `docs/known-gaps.md` /
`docs/design/query-demand-and-evidence.md`):

> Query results expose **rows plus scoped acquisition evidence, never global
> completeness or sync health.** … `Unknown` vs aggregate `CompleteUpTo` and the
> builder's authoritative-empty language must become **rows plus compact
> per-current-plan acquisition facts**. Diagnostics keep exact per-relay
> EOSE/watermark/AUTH/error evidence; **no public global completeness or
> `syncHealth` state remains.**

---

## 1. What exists today (the code being reworked)

| Layer | File | Role | Verdict |
|---|---|---|---|
| Durable evidence substrate | `nmp-store/src/coverage.rs` | `CoverageKey` (window-erased shape hash, 256-bit BLAKE3), `CoverageInterval{from,through}`, `record_coverage`/`get_coverage`/`merge_interval`/`shrink_after_eviction`, `ClaimSet`, GC | **KEEP UNCHANGED.** Already per-`(shape, relay)`; never made a global claim. |
| Evidence-gathering mechanism | `nmp-engine/src/core/attribution.rs` | `AttributionState`: send-time FIFO snapshots, intersection rule, `limit`-poisoning, wire-sub-id map, `shape_by_key` | **KEEP UNCHANGED.** This is how EOSE/NEG-DONE → watermark rows. Still needed to populate evidence. |
| **The collapse (the bug surface)** | `nmp-engine/src/core/coverage_query.rs` | `QueryCoverage{CompleteUpTo(Timestamp)\|Unknown}` + `query_coverage(atoms, plan, store)` — min-over-atoms-and-relays → one query-global verdict | **REWRITE.** This is the "authoritative-empty arriving through the derivation chain" (#12) and the "over-interprets relay evidence" global claim (#49). |
| Handle emit path | `nmp-engine/src/core/mod.rs` — `rows_and_coverage_for` (~L1506), `Effect::EmitRows(HandleId, Vec<RowDelta>, QueryCoverage)` (L183), `HandleState.last_coverage` (L214) | Computes coverage from `resolver.root_atoms(id)` **only** (#12 bug at L1510); ships it on every batch | **REWIRE.** Input widens root→subtree; value type changes. |
| Diagnostics (retained surface) | `nmp-engine/src/core/diagnostics.rs` — `FilterCoverageEntry{filter, coverage: QueryCoverage}` | Per-`(relay, filter)` coverage, **reuses** the query enum | **RETYPE.** Diagnostics legitimately keeps exact per-relay watermark evidence, but must stop borrowing the deleted query enum. |
| Public FFI/Swift/Kotlin | `nmp-ffi/src/{types,convert,facade,observer}.rs` — `FfiCoverage{CompleteUpTo{unix_seconds}\|Unknown}`, `FfiBatch.coverage`, `FfiFilterCoverage`, `on_batch(deltas, coverage)` | Projects `QueryCoverage` across the boundary | **REPLACE** (governed change). |

Key insight that shapes the whole plan: **the store never lied.** A
`CoverageInterval` at `(shape, relay)` is exactly-scoped, honest evidence. The
only place a per-relay/per-window fact was inflated into "your feed is complete"
is the `query_coverage` collapse and its FFI mirror. So this frame is a narrow
excision at one seam — **no persistence/redb schema change, no store migration.**

---

## 2. The scoped-evidence shape (the target)

Replace the single `QueryCoverage` verdict with a **per-current-plan list of
per-source acquisition facts** plus explicit shortfall. Facts, not judgment
(`query-demand-and-evidence.md` §3). Proposed Rust shape (names provisional,
closed set governed):

```rust
/// Compact acquisition evidence for one query snapshot. Scoped to THIS query's
/// own current demand + plan — never engine-global, never an authoritative
/// claim. No variant is named or documented complete / authoritative-empty /
/// synced / converged / syncHealth.
pub struct AcquisitionEvidence {
    /// One entry per relay in the query's CURRENT plan — the union of covering
    /// relays over every atom in the query's SUBTREE (interior atoms included,
    /// #12), not just the root atoms.
    pub sources: Vec<SourceAcquisition>,
    /// Subtree atoms with NO covering relay in the current plan (NoCandidates)
    /// and any local limit — the explicit, never-silent shortfall.
    pub shortfall: Vec<ShortfallFact>,
}

pub struct SourceAcquisition {
    pub relay: RelayUrl,
    pub state: SourceState,
}

/// The closed, honest per-source vocabulary. Compact labels; the EXACT
/// watermark/AUTH/error specifics stay in diagnostics.
pub enum SourceState {
    Requesting,                        // REQ outstanding, no proof yet for this query's atoms
    Reconciled { through: Timestamp }, // per-source watermark evidence — NOT "complete"
    // --- enrichment variants (see §2.1 on population) ---
    Connecting,
    Disconnected,
    Error,
    AwaitingAuth(AuthPhase),           // #8 evidence half
}

/// #8: AUTH state is part of per-relay acquisition evidence.
pub enum AuthPhase { AwaitingPolicy, AwaitingSignature, Authenticated, Denied }

pub enum ShortfallFact {
    NoCandidates { /* which subtree selection had no covering relay */ },
    LocalLimit  { /* explicit local cap that prevented intended acquisition */ },
}
```

Semantics vs the deleted enum:

- Old `CompleteUpTo(T)` (a query-global authoritative-empty) → **gone.** Its only
  honest residue is per-source `Reconciled{through}` — a *source* reconciled its
  window to a watermark. Any roll-up across sources is the app's interpretation,
  not NMP's claim.
- Old `Unknown` (any atom/relay unproven) → **gone as a verdict.** The unproven
  fact now shows locally: that source reads `Requesting`, or its atom appears in
  `shortfall` — the app sees *which* source is not yet settled, never a blanket
  "unknown."
- The empty row set is simply empty in the local replica. NMP never says synced /
  complete / authoritative-empty (`query-demand-and-evidence.md` §3 "No global
  completeness claim").

### 2.1 What this frame actually populates (honesty gate)

The engine can populate `Requesting`, `Reconciled{through}`, and
`shortfall::NoCandidates` **today**, purely from `plan` + `store` — a faithful,
lossless reshape of what `query_coverage` computed:

- covering relays per subtree atom come from `plan.reqs[*].absorbed.contains(key)`
  (unchanged logic);
- per source: `Reconciled{through = min over the subtree atoms that source
  covers}` iff every such atom has a `get_coverage` row with `from <=
  window_start`; else `Requesting`;
- a subtree atom with an empty covering set → `shortfall::NoCandidates` (the old
  "empty covering set ⇒ Unknown" branch, now a local fact).

`Connecting`/`Disconnected`/`Error` (from transport/router connection state) and
`AwaitingAuth(AuthPhase)` (#8) are **enrichment**: they require the engine to
fold in the same connection/AUTH state diagnostics already reads.
**Recommendation (owner Q3):** define the full closed enum now — so the closed
set is ratified once under governance — but document `Connecting/Disconnected/
Error/AwaitingAuth` as "reserved; populated when the transport-state fold and #8
wire half land." Adding a variant later is itself a governed surface change; do
it once.

### 2.2 Ratified vocabulary (codex-nova, this frame) — supersedes §2's names

U1+U2 (Rust core: `nmp-resolver`, `nmp-engine`) landed against this corrected
shape, ratified by codex-nova during build to resolve exactly the two defects
the Fable checkpoint below flags in §2's original draft (the watermark/link
conflation, and the AUTH vocabulary's representable non-states). This
supersedes §2's `SourceAcquisition`/`SourceState` sketch; `AcquisitionEvidence`
and `ShortfallFact` keep their §2 shape (with `NoCandidates` renamed
`NoPlannedSource` and a new `NoResolvedDemand` variant for a vacuously-empty
subtree):

```rust
pub struct AcquisitionEvidence { pub sources: Vec<SourceEvidence>, pub shortfall: Vec<ShortfallFact> }
pub struct SourceEvidence { pub relay: RelayUrl, pub reconciled_through: Option<Timestamp>, pub status: SourceStatus }
pub enum SourceStatus { Requesting, Connecting, Disconnected, AwaitingAuth { phase: AuthPhase }, AuthDenied, Error }
pub enum AuthPhase { AwaitingPolicy, AwaitingSignature }
pub enum ShortfallFact { NoPlannedSource { atom: ConcreteFilter }, NoResolvedDemand, LocalLimit { atom: ConcreteFilter } }
```

`reconciled_through` is a FIELD on `SourceEvidence`, never a `SourceStatus`
variant — the load-bearing fix: a source's durable proven watermark and its
current link status are orthogonal facts, so a relay can read
`reconciled_through: Some(_)` AND `status: Disconnected` in the very same
snapshot (the #49 "offline cached rows remain usable" acceptance criterion).
`AuthDenied` is its own top-level `SourceStatus`, never a phase of
`AwaitingAuth` (an enum that could express "awaiting-but-already-denied" would
be a representable non-state); `AuthPhase` keeps only `AwaitingPolicy`/
`AwaitingSignature` — no `Authenticated`/`Denied` phase, since an authenticated
source is just `Requesting`/carrying a `reconciled_through`.

Population in this frame (U1+U2 only, Rust core): `Requesting` (connected,
outstanding REQ), `Connecting` (planned, never yet connected this process),
and `Disconnected` (was connected, now dropped) are ALL populated — folded
from `EngineCore`'s own `connected_relays`/`ever_connected_relays` sets
(additive bookkeeping alongside the pre-existing `slot_to_url`, updated in
`on_relay_connected`/`on_relay_disconnected`). `AwaitingAuth`/`AuthDenied`
(#8) and `Error` (#51) remain reserved/unpopulated, as §2.1 already specified.

**#12 falsifiers landed** (`crates/nmp-engine/tests/core_headless.rs`):
`derived_query_evidence_surfaces_the_unproven_inner_atom_independently_of_the_outer`
(a `$myFollows`-shaped `Derived` query: the outer atom's relay proves its
window while the inner kind:3 atom's relay never does — the inner atom's
relay is PRESENT in `evidence.sources` with `reconciled_through: None`, then
flips to `Some` once its own EOSE lands) and
`source_watermark_survives_disconnect_alongside_the_disconnected_status` (the
orthogonality proof: `reconciled_through: Some(_)` and `status: Disconnected`
coexist on one `SourceEvidence` after a real connect-then-disconnect
sequence). `integration_capstone.rs::watermark_cold_start_offline` proves the
same orthogonality via a cold, offline restart instead (`status: Connecting`
+ `reconciled_through: Some(_)`, since that process never once connects to
the dead relay) — two independent falsifiers of the same fact via different
paths.

**Interior-vs-root heuristic (recorded durably, #12's general lesson):** for
ANY per-query mechanism — coverage/evidence, hint propagation (#11),
diagnostics attribution, GC claims — ask whether it behaves identically for
an interior (`Derived`'s own inner filter) node and a root node. Any "no" is
either a bug or an undocumented exception. `root_atoms` (rows) and
`subtree_atoms` (evidence) are deliberately DIFFERENT answers to that
question for DIFFERENT purposes — delivery shape stays root-only by design,
while every acquisition-evidence-shaped mechanism must consult the full
subtree, or it repeats #12's exact mistake.

---

## 3. The #12 fix (interior atoms) — folded into #49, not landed alone

**The bug:** `rows_and_coverage_for` (`mod.rs:1510`) feeds `resolver.root_atoms(id)`
into coverage, so a `$myFollows` query reports settled once the OUTER content
atoms are proven while the INNER kind:3 atom is still unproven — the derivation
chain's authoritative lie.

**The fix under the new shape:** build `AcquisitionEvidence.sources` over the
query's **subtree** atoms, not root atoms. The interior kind:3 atom's covering
relay then appears in `sources` (as `Requesting` until its row exists); it can
also lower a shared source's `Reconciled.through` via the min. Interior sources
are no longer invisible. Rows still come from `root_atoms` — delivery shape
unchanged, exactly as #12 requires.

**Mechanics:**
1. `nmp-resolver`: add `ResolverEngine::subtree_atoms(id) -> BTreeSet<ConcreteFilter>`
   — walk `graph.atoms_in_structural_order(root)` (the machinery already exists,
   `graph.rs:282`, currently used only for refcounting) and collect into a set.
   `root_atoms` stays for the row computation.
2. `rows_and_coverage_for`: rows from `root_atoms(id)`; evidence from
   `subtree_atoms(id)`.

**Ordering discipline (important):** #12's issue text prescribes the *old-model*
fix ("`query_coverage` aggregates over the subtree, min-over-subtree ⇒
`CompleteUpTo(min(inner,outer))`"). **Do NOT land that** as a separate patch —
it widens the input of a function this frame deletes, and it re-asserts the
`CompleteUpTo` collapse #49 removes. Fold #12 into #49: the evidence builder is
subtree-based from birth. The two issues close together.

**Reshaped falsifier** (the issue's `Unknown → CompleteUpTo(min)` test cannot
survive verbatim — the vocabulary is gone; #52 requires synchronized falsifiers):
subscribe a `Derived` query against a store where the outer atoms have coverage
rows but the inner atom has none →

- the inner atom's covering relay is PRESENT in `evidence.sources` and reads
  `Requesting` (proving interior atoms are consulted);
- no source is presented in a way that implies the feed is settled;
- add the inner row → that source flips to `Reconciled{through}`.

Also record #12's general heuristic in the durable doc: *for any mechanism
(coverage, hint propagation #11, diagnostics attribution, GC), does it behave
identically for an interior node and a root node? Any "no" is a bug or an
undocumented exception.*

---

## 4. What is deleted / migrated / retained

**Deleted (public, no compat alias — feedback: hard-break + update all callers
in one PR):**
- `QueryCoverage` (enum + `query_coverage` fn) as the query-result value.
- `FfiCoverage`, `FfiBatch.coverage`, and the `coverage` arg of `Observer::on_batch`.

**Migrated / rewired:**
- `coverage_query.rs::query_coverage` → `acquisition_evidence(subtree_atoms, plan,
  store) -> AcquisitionEvidence` (same reads, per-source output, subtree input).
- `Effect::EmitRows(HandleId, Vec<RowDelta>, QueryCoverage)` →
  `EmitRows(HandleId, Vec<RowDelta>, AcquisitionEvidence)`;
  `HandleState.last_coverage` → `last_evidence` (the change-detection compare at
  `mod.rs:1482` must compare evidence values — derive `PartialEq`).
- FFI: `FfiCoverage` → `FfiAcquisitionEvidence` (+ `FfiSourceAcquisition`,
  `FfiSourceState`, `FfiAuthPhase`, `FfiShortfallFact`); `coverage_to_ffi` →
  `evidence_to_ffi`; `on_batch` signature; Swift/Kotlin regenerated.

**Retained (the diagnostics surface — this is allowed and required):**
- `nmp-store::coverage` substrate: untouched. **No redb/persistence change.**
- `attribution.rs`: untouched.
- Diagnostics keeps exact per-`(relay, filter)` watermark evidence, but
  `FilterCoverageEntry` must stop reusing the deleted `QueryCoverage`. Retype its
  `coverage` field to a diagnostics-local fact (e.g. `Option<CoverageInterval>`
  rendered as reconciled-through / unproven), and add the AUTH/EOSE/error facts
  the contract says diagnostics retains. Diagnostics is engine-global and
  unscoped by design; scoped evidence is the *query* surface — the two are
  deliberately distinct (`query-demand-and-evidence.md` §4).

---

## 5. Surface governance (#52) — mandatory

This is a public Rust-facade + FFI + Swift + Kotlin shape change, so it must ride
the protocol `design-canonical-facade-52` / `build-52-a-facade` are standing up
(`docs/design/canonical-facade-52-plan.md` §5). Required in the same PR:

1. **`docs/surface-change-log.md` entry** (7 fields):
   - *Failure evidence:* #49, #12 (interior-atom lie), #8; the known-gaps
     "Current `Coverage` over-interprets relay evidence" bullet.
   - *Cross-surface impact:* Rust `QueryCoverage`→`AcquisitionEvidence`; FFI
     `FfiCoverage`→`FfiAcquisitionEvidence`; Swift/Kotlin `on_batch` signature.
   - *Persistence impact:* **NONE** — store coverage rows/redb keys unchanged
     (state this explicitly; it's the load-bearing containment of the change).
   - *Diagnostics impact:* `FilterCoverageEntry` retyped; per-relay watermark
     evidence retained, AUTH/EOSE/error facts added.
   - *Updated falsifiers:* reshaped `coverage_query` tests, reshaped #12
     falsifier (§3), parity-harness evidence assertions.
   - *Superseded path removed:* `QueryCoverage` + `FfiCoverage` deleted, no alias.
   - *Human signoff:* owner + PR/date.
2. **Regenerate `docs/surface/nmp-facade.txt` + `nmp-ffi.udl.txt`** → the diff is
   the governed artifact; the `surface-governance` CI gate fails if the snapshot
   moves without a same-PR log entry.
3. **Parity harness (`crates/nmp-parity`, #52 unit D):** its coverage assertions
   currently compare `FfiCoverage` across facade + FFI. They must be updated in
   lockstep to assert identical `AcquisitionEvidence`. **Coordinate directly with
   `build-52-a-facade` / `design-canonical-facade-52`** — this is the primary
   collision seam. Do not land the FFI reshape until the parity harness is
   reshaped with it, or the gate/parity breaks.

---

## 6. Collision-safe decomposition

One cohesive breaking change across crates with no compat alias allowed → **ONE
PR, ONE shared worktree** (feedback: cohesive feature = one PR/worktree). Sub-units
built by parallel agents in the SAME worktree, in dependency order:

| Unit | Crate / files | Depends on | Collision / coordination |
|---|---|---|---|
| **U1 — subtree accessor** | `nmp-resolver`: `subtree_atoms(id)` over `atoms_in_structural_order` | — | Internal crate, no governed surface. Isolated. |
| **U2 — evidence core (heart; folds #12)** | `nmp-engine/core/coverage_query.rs` rewrite → `acquisition_evidence`; new `AcquisitionEvidence`/`SourceAcquisition`/`SourceState`/`AuthPhase`/`ShortfallFact`; rewire `rows_and_coverage_for`, `Effect::EmitRows`, `HandleState` | U1 | Store READ path only (`get_coverage`) — **does not touch `nmp-store/coverage.rs`**, so minimal collision with #2/#3 store work (`build-23-*`, `build-store-internals`, `build-store-query-rescan`). Confirm no schema change with them. |
| **U3 — diagnostics retype** | `nmp-engine/core/diagnostics.rs`: `FilterCoverageEntry` off `QueryCoverage`; add AUTH/EOSE/error facts | U2 (type deletion) | Parallel with U4. |
| **U4 — FFI + observer** | `nmp-ffi/{types,convert,facade,observer}.rs`; Swift/Kotlin regen | U2 | Parallel with U3. **Governed surface** → §5. Coordinate `build-52-a-facade`, `build-kotlin-falsifier`, `build-swift-batching-cleanclone`. |
| **U5 — governance + falsifiers + parity** | `docs/surface-change-log.md`, snapshot regen, reshaped `coverage_query`/#12 falsifiers, `nmp-parity` evidence assertions, known-gaps bullet update, this doc's heuristic recorded durably | U2–U4 | **Coordinate `design-canonical-facade-52`** (parity + gate). |

Order: **U1 → U2 → {U3, U4} → U5.** Scope `cargo test -p nmp-resolver -p
nmp-engine -p nmp-ffi` + `nmp-parity` + the doctrine-lint smoke; never full
workspace (agent test-scoping rule).

---

## 7. Owner questions

1. **Per-source only, or also a query-level roll-up?** The contract says "rows +
   compact per-current-plan acquisition facts." A query-level `min-through` is a
   convenience but re-introduces the exact collapse #49 removes and risks reading
   as "complete." **Recommendation: per-source facts only; the app rolls up.** No
   query-global watermark on the public surface. Confirm.
2. **AUTH vocabulary timing (#8 evidence half).** Reserve `AwaitingAuth(AuthPhase)`
   in the enum now (populated when #8's wire half lands), or add it when #8
   lands? **Recommendation: reserve now** — #8 is a committed sibling in the same
   #43 step-5; adding it later is a second governed surface change for no benefit.
3. **Ratify the closed `SourceState` set.** It's a public governed enum; adding a
   variant later is a governed change. Proposed closed set: `Requesting`,
   `Reconciled{through}`, `Connecting`, `Disconnected`, `Error`,
   `AwaitingAuth(AuthPhase)`. Ship the full set now with the transport/AUTH
   variants documented "reserved / not-yet-populated" (per "always right, never
   smallest"), or ship only the populatable subset and treat each later variant
   as a governed change? **Recommendation: full set now, documented.**

Nothing in this plan invents beyond the contract: the shape is
`query-demand-and-evidence.md` §3's "compact facts scoped to the descriptor's
current planned sources," made concrete.

---

## Fable checkpoint (verdict)

**GO — with required changes.** The plan's diagnosis is correct and
code-verified, the excision is genuinely narrow at the engine layer, and the
#12 fold is the right move. Two things the plan got wrong must be fixed before
build: the proposed `SourceState` enum conflates two orthogonal facts (durable
watermark vs live link state — the contract's own "cached-only" fact is
inexpressible in it), and the caller inventory is incomplete (`nmp-bdd`, the
hand-written Swift/Kotlin SDK wrappers, and the in-flight `crates/nmp` facade
are all consumers the unit table misses).

### Narrow-excision claim — verified against code, with the leak list

The central claim **holds at the engine layer**:

- `nmp-store/src/coverage.rs` is exactly as described: keyed by window-erased
  shape hash per `(shape, relay)`, merge-only `record_coverage`, "no row = not
  covered" `get_coverage`, GC-only lowering. It never makes a global claim.
  **KEEP UNCHANGED — ratified.** Same for `attribution.rs` (engine decides
  whether/what to record; the store only merges what it is told).
- The ONLY place per-relay facts collapse into a query-global verdict is
  `coverage_query.rs::query_coverage` (min-over-atoms-and-relays, unanimity,
  empty-covering-set → `Unknown`) plus its projections. Confirmed by grep:
  no other code path constructs `CompleteUpTo` as a query-level claim.
- `rows_and_coverage_for` is at `core/mod.rs:1506-1520` (the issue's `:1414`
  drifted); it feeds `resolver.root_atoms(id)` only — #12 confirmed.
  `atoms_in_structural_order` exists at `graph.rs:282`, currently
  refcount-only — the U1 accessor is a straightforward collect.

But the **full consumer set is wider than the plan's unit table** (all must be
reshaped in the same PR; none are optional):

1. **`crates/nmp-bdd`** — `world.rs` (`World::apply(deltas, coverage)`, field
   `coverage: QueryCoverage`, `feed_eventually` predicates) and
   `steps/then.rs` consume `QueryCoverage` directly. Missing from the unit
   table entirely. Add to U5 (falsifier reshape) or a U3b.
2. **Hand-written SDK wrappers, not just regen:** `Packages/NMP/Sources/NMP/`
   (`Row.swift`'s public `Coverage` enum, `Query.swift` `onBatch`,
   `Observable.swift`'s `coverage` property, `Diagnostics.swift`
   `FilterCoverage`) and `Packages/NMPKotlin/.../` (`Row.kt` `Coverage`,
   `Query.kt` `onBatch`, `Diagnostics.kt`). "Swift/Kotlin regenerated"
   under-describes U4: `gen/` regenerates; these are hand-reshaped and are
   themselves governed surface.
3. **`crates/nmp` (the #52 facade, in flight now)** — it will expose the batch
   evidence value on the product surface. U4's scope must include it (see
   sequencing). The plan predates it; this is a missing unit.
4. **Engine integration falsifiers** carry semantics, not just types:
   `integration_capstone.rs`'s offline-authoritative-read phases,
   `core_headless.rs` §"per-query CompleteUpTo aggregation",
   `diagnostics_headless.rs`, `negentropy_live.rs`. Each must be re-expressed
   per-source with its underlying invariant preserved (see the watermark/link
   split below — the capstone is why the split is mandatory).
5. **Prose sweep:** doc comments referencing the deleted vocabulary survive
   compilation — `runtime/mod.rs:37-41`, `nmp-store/coverage.rs:56,309`,
   `nmp-store/lib.rs:226`, `nmp-grammar/concrete.rs:45`,
   `nmp-ffi/facade.rs:49` ("authoritative"). Sweep in U5.

`crates/nmp-demo` has zero coverage consumers — confirmed clean.

### The three owner decisions — resolved

**Q1 — Per-source facts only. RATIFIED, no query-level roll-up.** A
`min-through` convenience is the deleted collapse wearing a new name; the
contract's "never global completeness" forbids it and removing it loses
convenience, not information — apps fold source facts into their own progress
policy. Three teeth the builder must add:

- **No aggregate anywhere** — no helper fn, no computed property on the Swift/
  Kotlin wrappers either (an `isComplete` convenience in `Row.swift` would be
  the same collapse one layer up; the parity/governance review must watch the
  hand-written wrappers for exactly this).
- **Vacuous-emptiness guard:** a query whose subtree yields zero atoms or zero
  planned sources must read as explicit `shortfall`, never as an empty
  `sources` list an app can read as trivially settled. The old
  `atoms.is_empty() → Unknown` branch maps to a shortfall fact, not to
  nothing.
- **Recommended (not gating):** carry the plan revision the evidence was
  computed against, so apps can correlate compact evidence with the
  diagnostics stream's exact plan (§4's "current source plan and its
  revision").

**Q2 — Reserve the AUTH vocabulary now. YES, with a corrected shape.** #8 is a
committed sibling in the same #43 step-5; re-opening a governed enum later is
a second surface change for zero benefit. But the proposed
`AwaitingAuth(AuthPhase{AwaitingPolicy, AwaitingSignature, Authenticated,
Denied})` bakes two lies into a ratified vocabulary:
`AwaitingAuth(Authenticated)` is a representable non-state (an authenticated
source is just requesting/reconciled — authentication detail is diagnostics,
per #8's own contract), and `Denied` is terminal, not awaited. Required shape:
`AwaitingAuth { phase: AwaitingPolicy | AwaitingSignature }` plus a top-level
`AuthDenied` status. The full ladder (challenge/authenticated/replay) stays
diagnostics-only.

**Q3 — Ratify the closed set: YES, full set now — but the enum must be split
first.** The single `SourceState` enum conflates a **durable past fact** (a
persisted watermark) with a **current link fact** (connecting/disconnected/
auth-parked). These coexist: a relay with a persisted `through=T` that is
currently offline is the contract's own "cached-only" fact
(`query-demand-and-evidence.md` §3) and is exactly what
`integration_capstone.rs`'s offline-authoritative phase proves (#49
acceptance: "offline cached rows remain usable"). In a single enum, either
`Disconnected` shadows the watermark (the offline read loses its evidence) or
the watermark shadows the link state — both dishonest. **Required shape:**

```rust
pub struct SourceAcquisition {
    pub relay: RelayUrl,
    /// Durable per-(shape,relay) watermark evidence for the subtree atoms
    /// this source covers (min over them, iff every one has a row with
    /// from <= window floor). None = unproven. NOT "complete".
    pub reconciled_through: Option<Timestamp>,
    /// Current link/acquisition status — orthogonal to the watermark.
    pub status: SourceStatus,
}

pub enum SourceStatus {
    Requesting,    // sub open (pre- or post-proof; the watermark says which)
    Connecting,
    Disconnected,  // + Some(reconciled_through) == the contract's "cached-only"
    AwaitingAuth { phase: AuthPhase },  // #8, reserved
    AuthDenied,                          // #8, reserved
    Error,
}
pub enum AuthPhase { AwaitingPolicy, AwaitingSignature }
```

(Exact spellings are the builder's; the split, the corrected AUTH vocabulary,
and closedness are not.) Population honesty, resolving the reserved-variant
concern raised in review: this frame populates `reconciled_through`,
`Requesting`, and `shortfall` from `plan`+`store` as §2.1 says — **and also
`Connecting`/`Disconnected`**, because the core already owns that state
(`EngineMsg::RelayConnected/RelayDisconnected`, the slot map at
`core/mod.rs:259-261`); folding it is a read, not new plumbing. That leaves
exactly `AwaitingAuth`/`AuthDenied` reserved (named landing issue: #8) and
`Error` reserved-or-folded per what transport actually surfaces today. Rule:
every ratified variant is either populated in this PR or documented reserved
with a named issue — no vocabulary that nothing can ever emit and no issue
will ever populate.

### Contract validation

- **"Scoped evidence, never global completeness" — honored** under the
  amended shape. No hidden aggregate: `sources` + `shortfall` are per-source/
  per-atom facts; the vacuous-emptiness guard closes the one silent hole.
- **`reconciled_through` is honest** — read from per-(shape,relay) rows with
  the window-floor check, min'd only over the subtree atoms *this source
  covers in this query*. Document that scoping in the field doc verbatim.
- **The #12 fix closes the hole without re-collapse** — interior atoms'
  covering relays appear in `sources` (unproven ⇒ watermark `None`), rows
  still come from `root_atoms`, and no min crosses sources. The plan's
  ordering discipline (never land #12's old-model `CompleteUpTo(min)` patch)
  is correct — that patch would widen the input of a function this frame
  deletes. Fold and close both issues together, as written.
- **Hard delete with no compat alias is safe** — the full consumer set is the
  leak list above (nmp-ffi, nmp-bdd, engine tests, Swift+Kotlin wrappers,
  in-flight facade); all in-repo, one PR. No out-of-repo consumer exists yet
  (pre-v2, no published crates).
- **Determinism requirement (new):** `refresh_handle`'s change-detection at
  `core/mod.rs:1482` becomes a `PartialEq` compare on the evidence value.
  `sources` must have deterministic order (sort by relay URL) and stable
  construction, or every refresh emits a spurious batch. Derive
  `PartialEq/Eq`; add a falsifier: two consecutive refreshes with no
  state change emit nothing.

### Sequencing vs #52 and #2/#3

- **vs #2/#3 (crash-safe Accepted): fully parallel.** Confirmed no schema
  collision — this frame's store touches are `get_coverage` reads only; #2/#3
  adds new `OUTBOX_*` tables and doors and does not touch the COVERAGE table.
  The only overlap is textual in `core/mod.rs` (their seam: `on_publish`/
  outbox; ours: `refresh_handle`/`rows_and_coverage_for` — disjoint regions).
  Coordinate merge order, no design dependency either way.
- **vs #52: start U1–U3 now; merge after E+F land.** U1–U3 are engine-internal
  (under #52 everything below `nmp` is explicitly unstable — not governed
  surface). But the type deletion breaks `nmp-ffi` in-workspace, so the one PR
  necessarily includes U4, and U4 is a governed change. This frame must
  **not** improvise its own change-log file or snapshot format — two agents
  defining the governance artifact is a duplicate-plan violation; F owns the
  format, E owns the snapshots. Since E depends on A+B, the effective merge
  prerequisite is **#52 A0 → A → B → E → F landed** (all in flight, all
  small), with this frame's log entry riding the real protocol as its second
  real entry (after the #41 verify reshape). **D (parity) is NOT a hard
  prerequisite:** if `nmp-parity` exists by merge time, reshape its evidence
  assertions in lockstep (plan §5.3 stands); if not, D is simply built later
  on the final shape — cheaper, no interim churn — and this PR's falsifiers
  (engine + bdd + Swift/Kotlin tests) carry the burden. Add `crates/nmp` to
  U4's scope; coordinate with `build-52-a-facade` so the facade's query
  surface is reshaped once, not built on `Coverage` and immediately re-cut.
- **Build order inside the frame: unchanged** — U1 → U2 → {U3, U4} → U5, one
  shared worktree, one PR; test scope as written plus
  `Packages/NMP`/`NMPKotlin` test suites (the wrappers have their own tests:
  `DiagnosticsTests.swift` etc.).

### Required changes (summary)

1. Split watermark from link status in `SourceAcquisition` (Q3 shape above).
2. Fix the AUTH vocabulary: no `Authenticated` in evidence; `AuthDenied`
   top-level; phases = `AwaitingPolicy | AwaitingSignature` (Q2).
3. Populate `Connecting`/`Disconnected` in this frame (core already owns the
   state); only #8's variants stay reserved, documented with the issue number.
4. Add the missing consumers to the unit table: `nmp-bdd` (U5 or U3b),
   hand-written `Packages/NMP` + `Packages/NMPKotlin` wrappers (U4, explicit),
   `crates/nmp` facade (U4, coordinate with `build-52-a-facade`).
5. Vacuous-emptiness guard: zero atoms / zero planned sources ⇒ explicit
   shortfall, never an empty `sources` list.
6. Deterministic `sources` ordering + `PartialEq` + no-spurious-emit falsifier.
7. No roll-up anywhere, including no convenience aggregate on the Swift/Kotlin
   wrappers; reviewers watch for `isComplete`-shaped helpers.
8. Merge gate: after #52 units E+F exist; log entry + snapshot regen ride the
   real protocol. Parity lockstep only if `nmp-parity` exists by then.
9. Prose sweep of the deleted vocabulary in doc comments (leak list item 5);
   update the known-gaps "over-interprets relay evidence" bullet in U5 as
   planned.

### Residual risk

1. **`core/mod.rs` is the workspace's most contested file** — this frame, #52
   A0 (verify), and #2/#3 outbox all edit it within days. Regions are
   disjoint, but merge-order coordination is on the team lead; rebase, never
   force-push.
2. **Evidence compare cost:** `AcquisitionEvidence` is heap-allocated and
   compared on every `refresh_handle` (every event, every watermark advance).
   Sizes are small (sources ≈ planned relays), but if profiling ever shows it,
   the fix is a cheap revision counter, not a hash of a global claim — note
   for the builder, not a blocker.
3. **Subtree widening increases evidence-input size** for deep derived queries
   (the Magpie/depth-3 probe). Same asymptotics as today's `query_coverage`
   over a wider set; bounded by the demand graph. Acceptable.
4. **The wrappers can quietly reintroduce judgment** (a Swift `Coverage`-like
   enum "for ergonomics"). The governance review of the paired snapshot diff
   is the backstop; required change 7 names it so reviewers look.

— Fable, design checkpoint, 2026-07-11.
