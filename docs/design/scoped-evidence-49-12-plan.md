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
