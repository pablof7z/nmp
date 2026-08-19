# Scoped acquisition evidence — #49 / #12 / #8 (evidence half)

- **Status:** Shipped, as part of #43 step-5 (landed on #77). The cohesive
  Rust/FFI/Swift/Kotlin evidence wave, native mapping falsifiers, and scoped
  correctness proofs described below are built and merged.
- **Scope:** Replaced the engine-global `QueryCoverage::CompleteUpTo | Unknown`
  query-result value with **rows + compact, per-current-plan acquisition
  evidence**; fixed derived-query coverage to account for interior atoms (#12);
  reserved the AUTH phase in the per-source evidence vocabulary (#8 evidence half).
- **Issue disposition:** this cohesive wave closed #12 and advanced the evidence
  half of #49. It did **not** close #49: full descriptor identity
  (`selection + read routing + authenticated identity`) and context-isolated
  persistence/coalescing remain tracked there and in `docs/known-gaps.md`.
- **Nature: this was a REWORK, not a greenfield add.** The coverage-watermark
  substrate (`nmp-store::coverage`, `attribution.rs`) already existed and was
  correctly scoped; the collapse into a global claim lived in exactly one place
  (`coverage_query.rs`) plus its FFI projection. That is what this frame deleted.
- **Changed a public API** — the #49/#12 PR recorded the complete
  API delta in its body and updated synchronized falsifiers.

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
| Durable evidence substrate | `nmp-store/src/coverage.rs` | `CoverageKey` (window-erased shape hash, 256-bit BLAKE3), `CoverageInterval{from,through}`, `record_coverage`/`get_coverage`/`merge_interval`/`shrink_after_eviction`, `GcRetentionSet`, GC | **KEEP UNCHANGED.** Already per-`(shape, relay)`; never made a global claim. |
| Evidence-gathering mechanism | `nmp-engine/src/core/attribution.rs` | `AttributionState`: send-time FIFO snapshots, intersection rule, `limit`-poisoning, wire-sub-id map, `shape_by_key` | **KEEP UNCHANGED.** This is how EOSE/NEG-DONE → watermark rows. Still needed to populate evidence. |
| **The collapse (the bug surface)** | `nmp-engine/src/core/coverage_query.rs` | `QueryCoverage{CompleteUpTo(Timestamp)\|Unknown}` + `query_coverage(atoms, plan, store)` — min-over-atoms-and-relays → one query-global verdict | **REWRITE.** This is the "authoritative-empty arriving through the derivation chain" (#12) and the "over-interprets relay evidence" global claim (#49). |
| Handle emit path | `nmp-engine/src/core/mod.rs` — `rows_and_coverage_for` (~L1506), `Effect::EmitRows(HandleId, Vec<RowDelta>, QueryCoverage)` (L183), `HandleState.last_coverage` (L214) | Computes coverage from `resolver.root_atoms(id)` **only** (#12 bug at L1510); ships it on every batch | **REWIRE.** Input widens root→subtree; value type changes. |
| Diagnostics (retained surface) | `nmp-engine/src/core/diagnostics.rs` — `FilterCoverageEntry{filter, coverage: QueryCoverage}` | Per-`(relay, filter)` coverage, **reuses** the query enum | **RETYPE.** Diagnostics legitimately keeps exact per-relay watermark evidence, but must stop borrowing the deleted query enum. |
| Public FFI/Swift/Kotlin | `nmp-ffi/src/{types,convert,facade,observer}.rs` — `FfiCoverage{CompleteUpTo{unix_seconds}\|Unknown}`, `FfiBatch.coverage`, `FfiFilterCoverage`, `on_batch(deltas, coverage)` | Projects `QueryCoverage` across the boundary | **REPLACE** (breaking public-API change). |

Key insight that shapes the whole plan: **the store never lied.** A
`CoverageInterval` at `(shape, relay)` is exactly-scoped, honest evidence. The
only place a per-relay/per-window fact was inflated into "your feed is complete"
is the `query_coverage` collapse and its FFI mirror. So this frame is a narrow
excision at one seam — **no persistence/redb schema change, no store migration.**

---

## 2. The scoped-evidence shape

Replaced the single `QueryCoverage` verdict with a **per-current-plan list of
per-source acquisition facts** plus explicit shortfall. Facts, not judgment
(`query-demand-and-evidence.md` §3).

An earlier provisional shape in this section (`SourceAcquisition`/`SourceState`)
was replaced during build by the shape below, which resolves the two defects
the Fable checkpoint (below) identified in that draft: the watermark/link
conflation, and the AUTH vocabulary's representable non-states. `nmp-resolver`
and `nmp-engine` landed against this corrected shape; `AcquisitionEvidence`
and `ShortfallFact` keep the earlier draft's overall shape (with `NoCandidates`
renamed `NoPlannedSource` and a new `NoResolvedDemand` variant for a
vacuously-empty subtree):

```rust
pub struct AcquisitionEvidence { pub sources: Vec<SourceEvidence>, pub shortfall: Vec<ShortfallFact> }
pub struct SourceEvidence { pub relay: RelayUrl, pub reconciled_through: Option<Timestamp>, pub status: SourceStatus }
pub enum SourceStatus {
    Requesting,
    FinishedStoredEvents,
    AwaitingRequest,
    CoverageSatisfied,
    Connecting,
    Disconnected,
    AwaitingAuth { phase: AuthPhase },
    AuthDenied,
    Error,
}
pub enum AuthPhase {
    AwaitingChallenge,
    AwaitingPolicy,
    AwaitingSignature,
    AwaitingRelayAck,
}
pub enum ShortfallFact { NoPlannedSource { atom: ConcreteFilter }, NoResolvedDemand, LocalLimit { atom: ConcreteFilter } }
```

`reconciled_through` is a FIELD on `SourceEvidence`, never a `SourceStatus`
variant — the load-bearing fix: a source's durable proven watermark and its
current link status are orthogonal facts, so a relay can read
`reconciled_through: Some(_)` AND `status: Disconnected` in the very same
snapshot (the #49 "offline cached rows remain usable" acceptance criterion).
`AuthDenied` is its own top-level `SourceStatus`, never a phase of
`AwaitingAuth` (an enum that could express "awaiting-but-already-denied" would
be a representable non-state). `AuthPhase` names only the four outstanding
AUTH phases; completion, denial, and error remain top-level acquisition
states.

Population in the current implementation is exact and closed. `Requesting`
requires an accepted local request that is streaming; `FinishedStoredEvents`
requires that accepted request to reach EOSE. `AwaitingRequest` covers the
locally pending handoff and owned retry backoff before acceptance.
`CoverageSatisfied` belongs to a fresh MaxAge scope that owns no wire work and
is deliberately independent of link state. Live scopes use
`Connecting`/`Disconnected` until connected and ready, the four
`AwaitingAuth` phases while protected work is parked, `AuthDenied` for exact
denial, and `Error` for an exact source-local failure. No state is a
query-level completeness claim.

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
- FFI: `FfiCoverage` → `FfiAcquisitionEvidence` (+ `FfiSourceEvidence`,
  `FfiSourceStatus`, `FfiAuthPhase`, `FfiShortfallFact`); `coverage_to_ffi` →
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

## Design checkpoint (Fable, 2026-07-11)

Verified against code before build. The plan's diagnosis was correct and
code-verified, the excision was genuinely narrow at the engine layer, and the
#12 fold was the right move. Two corrections were made before build: the
`SourceState` enum in the original draft conflated two orthogonal facts
(durable watermark vs. live link state — the contract's own "cached-only"
fact was inexpressible in it), fixed by the `reconciled_through` field split
now in §2; and the caller inventory was wider than the original unit table
(`nmp-bdd`, the hand-written Swift/Kotlin SDK wrappers, and the in-flight
`crates/nmp` facade were all consumers reshaped in the same wave — see below).

### Narrow-excision claim — verified against code

- `nmp-store/src/coverage.rs` is exactly as described: keyed by window-erased
  shape hash per `(shape, relay)`, merge-only `record_coverage`, "no row = not
  covered" `get_coverage`, GC-only lowering. It never makes a global claim.
  Same for `attribution.rs` (engine decides whether/what to record; the store
  only merges what it is told).
- The ONLY place per-relay facts collapsed into a query-global verdict was
  `coverage_query.rs::query_coverage` (min-over-atoms-and-relays, unanimity,
  empty-covering-set → `Unknown`) plus its projections. Confirmed by grep: no
  other code path constructed `CompleteUpTo` as a query-level claim.
- `rows_and_coverage_for` fed `resolver.root_atoms(id)` only — the #12 bug,
  confirmed. `atoms_in_structural_order` (`graph.rs:282`) was already
  refcount-only machinery; the subtree accessor was a straightforward collect
  over it.

The full consumer set reshaped in this wave was wider than the plan's
original unit table: `crates/nmp-bdd` (`World::apply`, `feed_eventually`
predicates), the hand-written SDK wrappers (`Packages/NMP/Sources/NMP/`:
`Row.swift`, `Query.swift`, `Observable.swift`, `Diagnostics.swift`;
`Packages/NMPKotlin/.../`: `Row.kt`, `Query.kt`, `Diagnostics.kt`), the
`crates/nmp` facade, the engine integration falsifiers
(`integration_capstone.rs`, `core_headless.rs`, `diagnostics_headless.rs`,
`negentropy_live.rs`), and doc-comment prose referencing the deleted
vocabulary.

### The three owner decisions — resolved

**Q1 — Per-source facts only. RATIFIED, no query-level roll-up.** A
`min-through` convenience is the deleted collapse wearing a new name; the
contract's "never global completeness" forbids it and removing it loses
convenience, not information — apps fold source facts into their own progress
policy. Three teeth the builder must add:

- **No aggregate anywhere** — no helper fn, no computed property on the Swift/
  Kotlin wrappers either (an `isComplete` convenience in `Row.swift` would be
  the same collapse one layer up; the parity review must watch the
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
committed sibling in the same #43 step-5; re-opening a public enum later is
a second public-API change for zero benefit. But the proposed
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
    /// Effective current acquisition state — orthogonal to the watermark.
    pub status: SourceStatus,
}

pub enum SourceStatus {
    Requesting,              // exact local request accepted and streaming
    FinishedStoredEvents,    // accepted request reached EOSE
    AwaitingRequest,         // local handoff pending or retry scheduled
    CoverageSatisfied,       // fresh MaxAge scope; no wire request needed
    Connecting,
    Disconnected,  // + Some(reconciled_through) == the contract's "cached-only"
    AwaitingAuth { phase: AuthPhase },
    AuthDenied,
    Error,
}
pub enum AuthPhase {
    AwaitingChallenge,
    AwaitingPolicy,
    AwaitingSignature,
    AwaitingRelayAck,
}
```

(Exact spellings are the builder's; the split, the corrected AUTH vocabulary,
and closedness are not.) Population honesty: `Requesting` is emitted only
after exact local placement is accepted; a planned attempt or owned retry is
`AwaitingRequest`; accepted EOSE is `FinishedStoredEvents`; and a fresh
MaxAge scope suppressed from wire work is `CoverageSatisfied`, regardless of
link state. Live scopes populate `Connecting`, `Disconnected`, the four
`AwaitingAuth` phases, `AuthDenied`, and exact source-local `Error` from the
core's session/generation state. The durable `reconciled_through` field stays
orthogonal to every one of these effective current acquisition states.

### Contract validation

- **"Scoped evidence, never global completeness" — honored.** No hidden
  aggregate: `sources` + `shortfall` are per-source/per-atom facts; the
  vacuous-emptiness guard closes the one silent hole (a query whose subtree
  yields zero atoms or zero planned sources reads as explicit `shortfall`,
  never an empty `sources` list an app could read as trivially settled).
- **`reconciled_through` is honest** — read from per-(shape,relay) rows with
  the window-floor check, min'd only over the subtree atoms *this source
  covers in this query*.
- **The #12 fix closes the hole without re-collapse** — interior atoms'
  covering relays appear in `sources` (unproven ⇒ watermark `None`), rows
  still come from `root_atoms`, and no min crosses sources.
- **Hard delete with no compat alias was safe** — the full consumer set
  (nmp-ffi, nmp-bdd, engine tests, Swift+Kotlin wrappers, the in-flight
  facade) was all in-repo and all reshaped in one PR. No out-of-repo consumer
  existed (pre-v2, no published crates).
- **Determinism:** `AcquisitionEvidence`/`SourceEvidence` derive `PartialEq`/
  `Eq`, and `sources` construction is deterministic. The falsifier
  `equal_evidence_on_reconnect_does_not_spuriously_emit_rows`
  (`crates/nmp-engine/tests/core_headless/live_queries.rs`) proves two
  consecutive refreshes with no state change emit nothing.

### Risk noted at checkpoint

The hand-written SDK wrappers could quietly reintroduce judgment (a Swift/
Kotlin `Coverage`-like convenience "for ergonomics" — e.g. an `isComplete`
property — would be the same collapse one layer up). As shipped, no such
aggregate exists in `Row.swift` or `Row.kt`.

— Fable, design checkpoint, 2026-07-11.
