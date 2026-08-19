//! Typed ownership for live-wire demand: which handles own which atoms, and
//! which logical demands are therefore live on the wire.
//!
//! Ten maps used to sit directly on `CoreState`, maintained by *two*
//! hand-written algorithms that nothing checked against each other: an
//! incremental path (`retain_wire_atom_owner_with_effects` /
//! `release_wire_atom_owner`) and a wholesale `rebuild_wire_ownership` that
//! open-coded the same owner counting a second time — and, because the maps
//! were public to the whole reducer, opened with twelve consecutive `.clear()`
//! calls that had to be remembered by hand. Two of those twelve belonged to a
//! different owner entirely.
//!
//! Here there is one implementation. [`WireOwnership::retain`] is the only
//! place a wire owner count can increase, [`WireOwnership::release`] the only
//! place it can decrease, and a rebuild is `WireOwnership::default()` plus a
//! replay — so a new map cannot be forgotten by a reset that does not mention
//! it.
//!
//! ## What this owner deliberately does not do
//!
//! Activating the router, observing attribution, and maintaining the
//! author-outbox provider bridge are *consequences* of an ownership change,
//! not part of one. They stay with the coordinator. Every mutating method
//! therefore returns the fact needed to perform them ([`AtomRetained`],
//! [`AtomReleased`], [`HandleAtomRemoval`]) rather than reaching for a router
//! or an attribution table itself.
//!
//! Whether a live demand is *pending admission* is likewise a router question,
//! not an ownership question. This owner stores the answer and exposes it, but
//! never computes it.
//!
//! ## One refcount rule
//!
//! Every refcount here underflows loudly. Before this module, per-handle
//! refcounts asserted (`checked_sub(..).expect(..)`) while the owner counts
//! 200 lines away absorbed the same violation with `saturating_sub`, for no
//! stated reason. Two spellings of one invariant is one spelling too many;
//! a wire owner count that goes negative is a bug in this file, and silence
//! would only move the crash somewhere less informative. [`WireOwnership::retain`]'s
//! two increments are `checked_add(..).expect(..)` for the same reason
//! (#1774): a `usize` wraps silently on overflow in a release build, and an
//! asymmetric rule -- checked one way, plain the other -- is itself the kind
//! of unmotivated inconsistency this file exists to not have.
//!
//! ## Why `retain`'s frozen atom body is safe to freeze
//!
//! [`WireOwnership::retain`] keeps the FIRST retainer's atom body and only
//! ever overwrites its `routing_evidence` afterward (below); [`assert_consistent`]
//! only ever checks that field and the count, never the rest of the stored
//! atom. That is sound only because `DemandKey` -- `(coverage_key(atom),
//! since, until, limit)` -- is injective over everything else a `ContextualAtom`
//! carries: `coverage_key` hashes `{window_erase(filter), source, access,
//! evidence: ∅}`, and `window_erase` blanks exactly `since`/`until`/`limit`.
//! So two atoms sharing a `DemandKey` differ from each other only in
//! `routing_evidence`, and freezing the rest of the body is a consequence of
//! that fact in two other crates (`nmp-router`, `nmp-store`), not one this
//! file enforces itself. A future change that erases one more field from
//! `coverage_key` would keep `assert_consistent` passing while silently
//! handing the router a stale atom body.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use nmp_grammar::{ContextualAtom, RoutingEvidence};
use nmp_resolver::HandleId;
use nmp_router::DemandKey;
use nmp_store::CoverageKey;

/// Canonical durable claim identities for one exact logical atom. Lifecycle
/// ownership remains keyed by the original `DemandKey`; only coverage
/// refresh expands multi-author outbox atoms to their per-author claims.
fn coverage_claim_keys(atom: &ContextualAtom) -> BTreeSet<CoverageKey> {
    nmp_store::coverage_claim_atoms(atom)
        .iter()
        .map(nmp_store::coverage_key)
        .collect()
}

/// Drop one reference and report whether the key became unreferenced.
/// Drop one reference and report whether the key became unreferenced.
///
/// The key must be there. `is_some_and` used to turn "this handle's refcount
/// for a key it demonstrably owns is missing" into a quiet "nothing was
/// released", which is the mirror already being broken with nothing saying so.
fn release_ref<K: Ord + Clone + std::fmt::Debug>(refs: &mut BTreeMap<K, usize>, key: K) -> bool {
    let count = refs
        .get_mut(&key)
        .unwrap_or_else(|| panic!("a handle owning {key:?} owns a refcount for it"));
    *count = count
        .checked_sub(1)
        .expect("per-handle ownership refcount cannot underflow");
    let remove = *count == 0;
    if remove {
        refs.remove(&key);
    }
    remove
}

/// What one owner's arrival did to a logical demand.
pub(super) struct AtomRetained {
    pub(super) key: DemandKey,
    /// The demand's atom carrying the live union of every owner's routing
    /// evidence. This — not the caller's own atom — is what the router and
    /// attribution must be told about.
    pub(super) effective_atom: ContextualAtom,
    /// This owner is the first: the demand just became live on the wire.
    pub(super) first_owner: bool,
    /// A routing fact nobody else was contributing arrived with this owner.
    pub(super) evidence_grew: bool,
}

/// What one owner's departure did to a logical demand.
pub(super) enum AtomReleased {
    /// Owners remain. The router still wants the narrowed effective atom.
    Survived {
        key: DemandKey,
        effective_atom: ContextualAtom,
    },
    /// The last owner left. Nothing indexes this demand any more, and the
    /// caller owns closing it on the wire.
    Ownerless { final_atom: ContextualAtom },
    /// No owner count existed. Releasing an atom this owner never held is a
    /// no-op, not an error: resolver-reported closes and ordinary handle
    /// detach can both reach the same atom.
    Unowned,
}

/// What removing one handle's claim on one atom did, for the resolver-close
/// recovery path.
#[derive(Default)]
pub(super) struct HandleAtomRemoval {
    /// The handle really did own this atom. When false, nothing changed.
    pub(super) removed: bool,
    /// This handle's last reference to the logical demand went away. The
    /// caller owns deactivating that handle's request targets for it.
    pub(super) demand_released: bool,
    /// Coverage claim keys this removal had to examine, for the bench census.
    #[cfg(feature = "bench-instrumentation")]
    pub(super) claims_examined: usize,
}

#[cfg(feature = "bench-instrumentation")]
pub(super) struct WireOwnershipCounts {
    pub(super) pending_atoms: usize,
    pub(super) pending_resolver_closes: usize,
    pub(super) handles: usize,
    pub(super) demand_ref_handles: usize,
    pub(super) demand_ref_keys: usize,
    pub(super) demand_refs: usize,
    pub(super) coverage_ref_handles: usize,
    pub(super) coverage_ref_keys: usize,
    pub(super) coverage_refs: usize,
    pub(super) owner_keys: usize,
    pub(super) owner_refs: usize,
    pub(super) reverse_owner_keys: usize,
    pub(super) coverage_keys: usize,
    pub(super) coverage_edges: usize,
    pub(super) demand_keys: usize,
    pub(super) demand_edges: usize,
    pub(super) routing_evidence_keys: usize,
    pub(super) routing_evidence_facts: usize,
    pub(super) routing_evidence_refs: usize,
}

#[derive(Default)]
pub(super) struct WireOwnership {
    /// Immutable per-handle live-wire atoms.
    atoms_by_handle: HashMap<HandleId, BTreeSet<ContextualAtom>>,
    /// Exact per-handle multiplicity after `DemandKey` erases routing
    /// evidence.
    demand_refs_by_handle: HashMap<HandleId, BTreeMap<DemandKey, usize>>,
    /// Exact per-handle multiplicity of normalized durable claim keys.
    coverage_refs_by_handle: HashMap<HandleId, BTreeMap<CoverageKey, usize>>,
    /// Every live logical demand, its effective atom, and its exact owner
    /// count.
    owner_counts: BTreeMap<DemandKey, (ContextualAtom, usize)>,
    /// Exact per-demand ownership of routing facts erased by `DemandKey`. The
    /// aggregate atom in `owner_counts` always carries this map's live union,
    /// while each fact remains independently removable.
    routing_evidence_owner_counts: BTreeMap<DemandKey, BTreeMap<RoutingEvidence, usize>>,
    /// Exact reverse edge used only when a resolver call reports a handle
    /// drop that happened before core could run its ordinary detach path.
    handles_by_atom: BTreeMap<ContextualAtom, BTreeSet<HandleId>>,
    /// Exact evidence-refresh candidates by immutable coverage identity.
    handles_by_coverage: BTreeMap<CoverageKey, BTreeSet<HandleId>>,
    /// Exact request-phase refresh candidates by relay-lifecycle identity.
    /// Unlike coverage, this retains since/until/limit and includes both
    /// ordinary observations and history handles.
    handles_by_demand: BTreeMap<DemandKey, BTreeSet<HandleId>>,
    /// Resolver-reported final closes wait until the current open transaction
    /// has attached any replacement live owner. That lets close+open of the
    /// same atom reattach to retained physical coverage without wire churn.
    pending_resolver_closes: BTreeMap<DemandKey, ContextualAtom>,
    /// Active exact demand not yet covered on every required physical
    /// session. Refused opens stay here until a real close frees capacity.
    /// Membership is a *router* verdict this owner stores but never computes.
    pending_atoms: BTreeMap<DemandKey, ContextualAtom>,
}

impl WireOwnership {
    // -- owner counting: the only two doors ------------------------------

    /// Add one owner of `atom`'s logical demand.
    pub(super) fn retain(&mut self, atom: &ContextualAtom) -> AtomRetained {
        let key = DemandKey::for_atom(atom);
        let evidence = self.routing_evidence_owner_counts.entry(key.clone()).or_default();
        let mut evidence_grew = false;
        for fact in &atom.routing_evidence {
            let count = evidence.entry(fact.clone()).or_insert(0);
            evidence_grew |= *count == 0;
            *count = count
                .checked_add(1)
                .expect("routing-evidence owner count cannot overflow");
        }
        let effective_evidence = evidence.keys().cloned().collect();

        let entry = self
            .owner_counts
            .entry(key.clone())
            .or_insert_with(|| (atom.clone(), 0));
        entry.1 = entry
            .1
            .checked_add(1)
            .expect("wire owner count cannot overflow");
        entry.0.routing_evidence = effective_evidence;
        AtomRetained {
            key,
            effective_atom: entry.0.clone(),
            first_owner: entry.1 == 1,
            evidence_grew,
        }
    }

    /// Remove one owner of `atom`'s logical demand.
    pub(super) fn release(&mut self, atom: &ContextualAtom) -> AtomReleased {
        let key = DemandKey::for_atom(atom);
        // Decide `Unowned` BEFORE touching routing evidence.
        //
        // The two maps are created and destroyed together, so an unowned key
        // has no evidence entry either -- but deciding ownership second meant
        // this function could mutate the evidence mirror and then report that
        // nothing was owned, and it forced the mirror to tolerate being
        // absent. That tolerance is what cost the ability to demand it is
        // present.
        if !self.owner_counts.contains_key(&key) {
            debug_assert!(
                !self.routing_evidence_owner_counts.contains_key(&key),
                "routing evidence outlived its demand's owner count"
            );
            return AtomReleased::Unowned;
        }
        let evidence = self
            .routing_evidence_owner_counts
            .get_mut(&key)
            .expect("an owned demand has a routing-evidence mirror");
        for fact in &atom.routing_evidence {
            let count = evidence
                .get_mut(fact)
                .expect("a released atom's routing fact is indexed by its demand");
            *count = count
                .checked_sub(1)
                .expect("routing-evidence owner count cannot underflow");
            if *count == 0 {
                evidence.remove(fact);
            }
        }
        let effective_evidence: BTreeSet<_> = evidence.keys().cloned().collect();

        let (effective_atom, count) = self
            .owner_counts
            .get_mut(&key)
            .expect("checked present above");
        *count = count
            .checked_sub(1)
            .expect("wire owner count cannot underflow");
        if *count == 0 {
            let final_atom = effective_atom.clone();
            self.owner_counts.remove(&key);
            self.routing_evidence_owner_counts.remove(&key);
            self.pending_atoms.remove(&key);
            self.pending_resolver_closes.remove(&key);
            return AtomReleased::Ownerless { final_atom };
        }
        effective_atom.routing_evidence = effective_evidence;
        AtomReleased::Survived {
            key,
            effective_atom: effective_atom.clone(),
        }
    }

    // -- per-handle indexing ---------------------------------------------

    /// Index one handle's complete live-wire atom set. Owner counting is the
    /// caller's separate [`Self::retain`] loop, because each retained atom
    /// produces a router and attribution consequence this owner cannot
    /// perform.
    ///
    /// Refuses a handle that is already indexed, rather than self-healing.
    /// An earlier version called [`Self::unindex_handle`] here and DROPPED
    /// the returned atom set -- exactly what the caller is contractually
    /// required to [`Self::release`] (the two `admission_tests/resolver_delta.rs`
    /// fixtures that used to call the now-deleted test-only `reindex_handle`
    /// spell out the correct sequence: unindex, release whatever left the
    /// set, index, retain whatever the set gained). The old self-heal
    /// silently traded index corruption for an `owner_counts` leak: every
    /// one of the old atoms stayed retained forever while its index edges
    /// vanished. Double-indexing is not a supported transition -- every
    /// `attach_wire_handle` call site passes a freshly minted `HandleId`
    /// (#1774) -- so this is the same refusal `owner_index.rs::insert` makes
    /// of a reused child id, for the same reason: a defensive branch that
    /// swaps one silent corruption for another is worse than a panic, and
    /// self-healing here would hide the caller bug that produced the
    /// double-index in the first place.
    pub(super) fn index_handle(&mut self, id: HandleId, atoms: BTreeSet<ContextualAtom>) {
        assert!(
            !self.atoms_by_handle.contains_key(&id),
            "WireOwnership: handle {id:?} is already indexed"
        );
        let mut demand_refs: BTreeMap<DemandKey, usize> = BTreeMap::new();
        let mut coverage_refs: BTreeMap<CoverageKey, usize> = BTreeMap::new();
        for atom in &atoms {
            let demand = DemandKey::for_atom(atom);
            *demand_refs.entry(demand.clone()).or_insert(0) += 1;
            self.handles_by_demand.entry(demand.clone()).or_default().insert(id);
            for claim_key in coverage_claim_keys(atom) {
                *coverage_refs.entry(claim_key.clone()).or_insert(0) += 1;
                self.handles_by_coverage
                    .entry(claim_key.clone())
                    .or_default()
                    .insert(id);
            }
            self.handles_by_atom
                .entry(atom.clone())
                .or_default()
                .insert(id);
        }
        self.atoms_by_handle.insert(id, atoms);
        self.demand_refs_by_handle.insert(id, demand_refs);
        self.coverage_refs_by_handle.insert(id, coverage_refs);
    }

    /// Drop every index edge for one handle and return the atoms it owned, so
    /// the caller can release each one.
    pub(super) fn unindex_handle(&mut self, id: HandleId) -> BTreeSet<ContextualAtom> {
        let atoms = self.atoms_by_handle.remove(&id).unwrap_or_default();
        let demand_refs = self.demand_refs_by_handle.remove(&id).unwrap_or_default();
        let coverage_refs = self.coverage_refs_by_handle.remove(&id).unwrap_or_default();
        for demand in demand_refs.keys() {
            discard_edge(&mut self.handles_by_demand, demand, id);
        }
        for claim_key in coverage_refs.keys() {
            discard_edge(&mut self.handles_by_coverage, claim_key, id);
        }
        for atom in &atoms {
            discard_edge(&mut self.handles_by_atom, atom, id);
        }
        atoms
    }

    /// Take every handle currently indexed under one exact atom, for the
    /// resolver-close recovery path.
    pub(super) fn take_handles_for_atom(
        &mut self,
        atom: &ContextualAtom,
    ) -> Option<BTreeSet<HandleId>> {
        self.handles_by_atom.remove(atom)
    }

    /// Remove one handle's claim on one atom without touching its siblings.
    ///
    /// Used only by the resolver-close recovery path, where the resolver has
    /// reported a close for an atom whose handle never ran ordinary detach.
    /// `handles_by_atom` is not touched here — the caller already took the
    /// whole set with [`Self::take_handles_for_atom`].
    pub(super) fn unindex_handle_atom(
        &mut self,
        handle: HandleId,
        atom: &ContextualAtom,
        key: DemandKey,
    ) -> HandleAtomRemoval {
        let departing_claims = coverage_claim_keys(atom);
        let atoms = self
            .atoms_by_handle
            .get_mut(&handle)
            .expect("a handle named by the atom reverse index owns an atom set");
        let removed = atoms.remove(atom);
        let handle_emptied = atoms.is_empty();
        if !removed {
            return HandleAtomRemoval::default();
        }

        let demand_released = release_ref.clone()(
            self.demand_refs_by_handle
                .get_mut(&handle)
                .expect("a handle owning atoms owns per-demand refcounts"),
            key.clone(),
        );
        if demand_released {
            discard_edge(&mut self.handles_by_demand, &key, handle);
        }
        #[cfg(feature = "bench-instrumentation")]
        let claims_examined = departing_claims.len();
        for claim_key in departing_claims {
            let released = release_ref.clone()(
                self.coverage_refs_by_handle
                    .get_mut(&handle)
                    .expect("a handle owning atoms owns per-claim refcounts"),
                claim_key.clone(),
            );
            if released {
                discard_edge(&mut self.handles_by_coverage, &claim_key, handle);
            }
        }
        if handle_emptied {
            self.atoms_by_handle.remove(&handle);
            self.demand_refs_by_handle.remove(&handle);
            self.coverage_refs_by_handle.remove(&handle);
        }
        HandleAtomRemoval {
            removed,
            demand_released,
            #[cfg(feature = "bench-instrumentation")]
            claims_examined,
        }
    }

    // -- reads ------------------------------------------------------------

    /// The exact atom union currently owned by handles whose immutable
    /// per-Demand opening-time freshness decision is `Live`. Suppressed
    /// Demand scopes still own their graph and cache projection, but their
    /// atoms are absent from this wire truth.
    pub(super) fn live_demand(&self) -> BTreeSet<ContextualAtom> {
        self.owner_counts
            .values()
            .map(|(atom, _)| atom.clone())
            .collect()
    }

    /// Every live logical demand with its effective atom.
    pub(super) fn live_demands(&self) -> impl Iterator<Item = (DemandKey, &ContextualAtom)> {
        self.owner_counts
            .iter()
            .map(|(key, (atom, _))| (key.clone(), atom))
    }

    /// Every live demand's effective atom and exact owner count.
    ///
    /// The one read the write plane takes from this owner: the neutral
    /// author-route provider bridge folds these counts into per-author
    /// totals. It is a snapshot, not a borrow, because the caller mutates
    /// its own state while walking it.
    pub(super) fn owner_contributions(&self) -> Vec<(ContextualAtom, usize)> {
        self.owner_counts
            .values()
            .map(|(atom, count)| (atom.clone(), *count))
            .collect()
    }

    pub(super) fn is_attached(&self, id: HandleId) -> bool {
        self.atoms_by_handle.contains_key(&id)
    }

    /// Whether some handle is already indexed as owning this exact atom.
    ///
    /// The precondition for retaining an owner. Retaining can refresh evidence,
    /// and that refresh reads the reverse indexes -- so an atom retained before
    /// its handle is indexed is one whose own arrival cannot see it.
    pub(super) fn is_indexed(&self, atom: &ContextualAtom) -> bool {
        self.handles_by_atom.contains_key(atom)
    }

    pub(super) fn handles_for_coverage(&self, key: &CoverageKey) -> Option<&BTreeSet<HandleId>> {
        self.handles_by_coverage.get(key)
    }

    pub(super) fn handles_for_demand(&self, key: &DemandKey) -> Option<&BTreeSet<HandleId>> {
        self.handles_by_demand.get(key)
    }

    // -- pending admission: stored here, decided by the router -------------

    pub(super) fn admission_needed(&self) -> bool {
        !self.pending_atoms.is_empty()
    }

    pub(super) fn pending_atoms(&self) -> impl Iterator<Item = &ContextualAtom> {
        self.pending_atoms.values()
    }

    pub(super) fn is_pending(&self, key: &DemandKey) -> bool {
        self.pending_atoms.contains_key(key)
    }

    pub(super) fn mark_pending(&mut self, key: DemandKey, atom: ContextualAtom) {
        self.pending_atoms.insert(key, atom);
    }

    pub(super) fn clear_pending(&mut self, key: &DemandKey) {
        self.pending_atoms.remove(key);
    }

    pub(super) fn replace_pending(&mut self, pending: BTreeMap<DemandKey, ContextualAtom>) {
        self.pending_atoms = pending;
    }

    // -- deferred resolver closes ------------------------------------------

    pub(super) fn defer_close(&mut self, key: DemandKey, atom: ContextualAtom) {
        self.pending_resolver_closes.insert(key, atom);
    }

    pub(super) fn clear_deferred_close(&mut self, key: &DemandKey) {
        self.pending_resolver_closes.remove(key);
    }

    pub(super) fn take_deferred_closes(&mut self) -> Vec<ContextualAtom> {
        std::mem::take(&mut self.pending_resolver_closes)
            .into_values()
            .collect()
    }

    // -- census -------------------------------------------------------------

    #[cfg(feature = "bench-instrumentation")]
    pub(super) fn counts(&self) -> WireOwnershipCounts {
        WireOwnershipCounts {
            pending_atoms: self.pending_atoms.len(),
            pending_resolver_closes: self.pending_resolver_closes.len(),
            handles: self.atoms_by_handle.len(),
            demand_ref_handles: self.demand_refs_by_handle.len(),
            demand_ref_keys: self.demand_refs_by_handle.values().map(BTreeMap::len).sum(),
            demand_refs: self
                .demand_refs_by_handle
                .values()
                .flat_map(BTreeMap::values)
                .sum(),
            coverage_ref_handles: self.coverage_refs_by_handle.len(),
            coverage_ref_keys: self
                .coverage_refs_by_handle
                .values()
                .map(BTreeMap::len)
                .sum(),
            coverage_refs: self
                .coverage_refs_by_handle
                .values()
                .flat_map(BTreeMap::values)
                .sum(),
            owner_keys: self.owner_counts.len(),
            owner_refs: self.owner_counts.values().map(|(_, count)| count).sum(),
            reverse_owner_keys: self.handles_by_atom.len(),
            coverage_keys: self.handles_by_coverage.len(),
            coverage_edges: self.handles_by_coverage.values().map(BTreeSet::len).sum(),
            demand_keys: self.handles_by_demand.len(),
            demand_edges: self.handles_by_demand.values().map(BTreeSet::len).sum(),
            routing_evidence_keys: self.routing_evidence_owner_counts.len(),
            routing_evidence_facts: self
                .routing_evidence_owner_counts
                .values()
                .map(BTreeMap::len)
                .sum(),
            routing_evidence_refs: self
                .routing_evidence_owner_counts
                .values()
                .flat_map(BTreeMap::values)
                .sum(),
        }
    }
}

/// Exact structural consistency, rebuilt from the one canonical relation.
///
/// `atoms_by_handle` is the truth: everything else in this owner is derivable
/// from it. So the check derives all of it and demands exact equality, rather
/// than comparing sizes.
///
/// The distinction is the whole point. A census that agrees on counts still
/// passes when a handle is indexed under the wrong atom, an atom under the
/// wrong demand, or a routing fact under the wrong key — corruption that
/// preserves cardinality. `wire_owner_refs` was added because the census could
/// not see a doubled owner count; this exists because it cannot see a
/// *misplaced* one either.
///
/// Two relations are deliberately NOT derived, because they are answers this
/// owner stores rather than computes:
///
/// - `pending_atoms` is a router verdict. Only its domain is checked (every
///   pending key is a live demand).
/// - `pending_resolver_closes` names demands that became ownerless, so its
///   keys must be disjoint from the live ones.
#[cfg(feature = "bench-instrumentation")]
impl WireOwnership {
    pub(super) fn assert_consistent(&self, at: &str) {
        let mut expected_demand_refs: HashMap<HandleId, BTreeMap<DemandKey, usize>> =
            HashMap::new();
        let mut expected_coverage_refs: HashMap<HandleId, BTreeMap<CoverageKey, usize>> =
            HashMap::new();
        let mut expected_by_atom: BTreeMap<ContextualAtom, BTreeSet<HandleId>> = BTreeMap::new();
        let mut expected_by_demand: BTreeMap<DemandKey, BTreeSet<HandleId>> = BTreeMap::new();
        let mut expected_by_coverage: BTreeMap<CoverageKey, BTreeSet<HandleId>> = BTreeMap::new();
        let mut expected_owner_counts: BTreeMap<DemandKey, usize> = BTreeMap::new();
        let mut expected_evidence: BTreeMap<DemandKey, BTreeMap<RoutingEvidence, usize>> =
            BTreeMap::new();

        for (id, atoms) in &self.atoms_by_handle {
            let demand_refs = expected_demand_refs.entry(*id).or_default();
            let coverage_refs = expected_coverage_refs.entry(*id).or_default();
            for atom in atoms {
                let key = DemandKey::for_atom(atom);
                *demand_refs.entry(key.clone()).or_insert(0) += 1;
                *expected_owner_counts.entry(key.clone()).or_insert(0) += 1;
                let evidence = expected_evidence.entry(key.clone()).or_default();
                for fact in &atom.routing_evidence {
                    *evidence.entry(fact.clone()).or_insert(0) += 1;
                }
                expected_by_atom
                    .entry(atom.clone())
                    .or_default()
                    .insert(*id);
                expected_by_demand.entry(key).or_default().insert(*id);
                for claim_key in coverage_claim_keys(atom) {
                    *coverage_refs.entry(claim_key.clone()).or_insert(0) += 1;
                    expected_by_coverage
                        .entry(claim_key)
                        .or_default()
                        .insert(*id);
                }
            }
        }

        assert_eq!(
            self.demand_refs_by_handle, expected_demand_refs,
            "{at}: per-handle demand refcounts do not match the atoms the handles own"
        );
        assert_eq!(
            self.coverage_refs_by_handle, expected_coverage_refs,
            "{at}: per-handle coverage refcounts do not match the atoms the handles own"
        );
        assert_eq!(
            self.handles_by_atom, expected_by_atom,
            "{at}: the atom reverse index names the wrong handles"
        );
        assert_eq!(
            self.handles_by_demand, expected_by_demand,
            "{at}: the demand reverse index names the wrong handles"
        );
        assert_eq!(
            self.handles_by_coverage, expected_by_coverage,
            "{at}: the coverage reverse index names the wrong handles"
        );

        // Owner counts are reachable exactly one way in production -- one
        // retain per atom per handle -- so they must equal the per-handle
        // totals key by key, not merely sum to the same number.
        let actual_owner_counts: BTreeMap<_, _> = self
            .owner_counts
            .iter()
            .map(|(key, (_, count))| (key.clone(), *count))
            .collect();
        assert_eq!(
            actual_owner_counts, expected_owner_counts,
            "{at}: wire owner counts do not match the handles owning each demand"
        );
        assert_eq!(
            self.routing_evidence_owner_counts, expected_evidence,
            "{at}: routing-evidence owner counts do not match the atoms contributing them"
        );

        // Each live demand presents the exact union of its owners' evidence.
        for (key, (atom, _)) in &self.owner_counts {
            let union: BTreeSet<_> = expected_evidence
                .get(key)
                .into_iter()
                .flat_map(BTreeMap::keys)
                .cloned()
                .collect();
            assert_eq!(
                atom.routing_evidence, union,
                "{at}: a demand's effective atom does not carry its owners' evidence union"
            );
        }

        for key in self.pending_atoms.keys() {
            assert!(
                self.owner_counts.contains_key(key),
                "{at}: a demand is pending admission with no live owner"
            );
        }
        for key in self.pending_resolver_closes.keys() {
            assert!(
                !self.owner_counts.contains_key(key),
                "{at}: a demand is awaiting a deferred close while still owned"
            );
        }
    }
}

/// Drop one handle from a reverse index, removing the key when it empties.
///
/// Both the key and the edge must be there. This used to be an `if let` that
/// shrugged at a missing key and never checked whether the handle was actually
/// removed, which is the precise shape of "the mirror is already broken and
/// nothing says so".
fn discard_edge<K: Ord + Clone + std::fmt::Debug>(
    index: &mut BTreeMap<K, BTreeSet<HandleId>>,
    key: &K,
    handle: HandleId,
) {
    let handles = index
        .get_mut(key)
        .unwrap_or_else(|| panic!("a handle's own index entry is missing for {key:?}"));
    assert!(
        handles.remove(&handle),
        "reverse index for {key:?} did not name the handle releasing it"
    );
    if handles.is_empty() {
        index.remove(key);
    }
}

