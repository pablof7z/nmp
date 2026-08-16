//! Typed ownership for observation execution targets: which branch executes
//! which filter path against which logical demand, and which of those are
//! currently active on the wire.
//!
//! Three maps in two layers. `by_handle` is what each branch *declares* —
//! every execution target it owns, active or not. `active_by_handle_demand`
//! and `by_demand` are what is currently *live*, derived from the declaration
//! by intersecting it with the branch's wire-contributing scopes. Keeping the
//! two layers apart is the whole reason this owner exists: a branch whose
//! freshness decision suppresses a scope still owns its declaration, and must
//! get it back unchanged when the scope contributes again.
//!
//! ## What made this an owner
//!
//! The wire-ownership rebuild used to reach across and clear two of these
//! three maps by hand, as part of a run of twelve `.clear()` calls it also
//! had to remember. That operation has a name — forget every activation,
//! keep every declaration — and it is [`RequestTargets::forget_activations`]
//! here. A rebuild that clears one of the two and not the other is no longer
//! expressible.
//!
//! ## What this owner does not know
//!
//! Which of a branch's scopes contribute to the wire is a *freshness*
//! decision owned by the branch, not by its execution targets. It arrives as
//! a passed-in set of scope indexes; this owner never reaches for a handle's
//! acquisition state to work it out.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use nmp_resolver::HandleId;
use nmp_router::DemandKey;

/// One current filter-resolution owner below a branch handle.
///
/// Exact relay demand and acquisition-scope identity are indexed separately
/// from the execution target. Distinct windows must never alias through their
/// shared durable coverage key, and two structural Demand occurrences may
/// resolve the same exact relay atom while only one owns wire participation.
/// The multiplicity in the owning maps makes replacement and teardown exact
/// without rescanning remembered resolver nodes.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct ActiveRequestTarget {
    pub(super) demand: DemandKey,
    pub(super) scope: usize,
    pub(super) path: String,
    pub(super) revision: u64,
}

/// Reverse-index value for one current observation execution target.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct RequestTarget {
    pub(super) handle: HandleId,
    pub(super) path: String,
    pub(super) revision: u64,
}

/// What walking one demand's live targets had to examine, for the bench
/// census. Returned rather than counted in place so this owner holds no
/// instrumentation cells of its own.
#[derive(Default)]
pub(super) struct DemandWalk {
    pub(super) demand_keys_touched: u64,
    pub(super) candidates_examined: u64,
}

/// The census contribution, so the root counts this owner's state without
/// naming its maps.
#[cfg(any(test, feature = "bench-instrumentation"))]
pub(super) struct RequestTargetCounts {
    pub(super) handles: usize,
    pub(super) demand_keys: usize,
    pub(super) edges: usize,
    pub(super) refs: usize,
    pub(super) active_handles: usize,
    pub(super) active_demand_keys: usize,
    pub(super) active_edges: usize,
    pub(super) active_refs: usize,
}

#[derive(Default)]
pub(super) struct RequestTargets {
    /// Complete current-snapshot request-evidence edges per ordinary handle.
    /// A reconcile replaces only this handle's set, so paths absent from the
    /// new resolver snapshot and stale revisions disappear immediately.
    by_handle: HashMap<HandleId, BTreeMap<ActiveRequestTarget, usize>>,
    /// Exact logical demand -> wire-active execution targets reverse index.
    /// REQ attribution retains since/until/limit and never aliases observers
    /// merely because durable coverage erases those fields.
    by_demand: BTreeMap<DemandKey, BTreeMap<RequestTarget, usize>>,
    /// Wire-active request targets grouped by exact handle and DemandKey.
    /// Partial resolver closes can remove one demand's edges without scanning
    /// or dropping sibling execution targets owned by the same handle.
    active_by_handle_demand: HashMap<HandleId, BTreeMap<DemandKey, BTreeMap<RequestTarget, usize>>>,
}

impl RequestTargets {
    // -- declarations -------------------------------------------------------

    /// Replace one handle's declared target set.
    ///
    /// `active_scopes` is `None` when the handle owns no live wire
    /// participation: its declaration is stored but nothing is activated, and
    /// nothing was active to release. Passing `Some` re-derives the active
    /// layer from the new declaration in one step, which is what keeps a
    /// deactivate/replace/activate sequence from being three separate things
    /// a caller has to remember to do in order.
    pub(super) fn replace_for_handle(
        &mut self,
        id: HandleId,
        declared: BTreeMap<ActiveRequestTarget, usize>,
        active_scopes: Option<&BTreeSet<usize>>,
    ) {
        if active_scopes.is_some() {
            self.deactivate_handle(id);
        }
        self.by_handle.remove(&id);
        if !declared.is_empty() {
            self.by_handle.insert(id, declared);
        }
        if let Some(scopes) = active_scopes {
            self.activate_handle(id, scopes);
        }
    }

    /// Every handle with a declared target set.
    pub(super) fn declared_handles(&self) -> Vec<HandleId> {
        self.by_handle.keys().copied().collect()
    }

    // -- activation ---------------------------------------------------------

    /// Make live every declared target of `id` whose scope contributes wire.
    pub(super) fn activate_handle(&mut self, id: HandleId, active_scopes: &BTreeSet<usize>) {
        let Some(declared) = self.by_handle.get(&id) else {
            return;
        };
        let mut active_by_demand: BTreeMap<_, BTreeMap<_, usize>> = BTreeMap::new();
        for (target, count) in declared {
            if !active_scopes.contains(&target.scope) {
                continue;
            }
            let reverse_target = RequestTarget {
                handle: id,
                path: target.path.clone(),
                revision: target.revision,
            };
            *active_by_demand
                .entry(target.demand)
                .or_default()
                .entry(reverse_target)
                .or_insert(0) += count;
        }
        for (demand, targets) in &active_by_demand {
            let indexed = self.by_demand.entry(*demand).or_default();
            for (target, count) in targets {
                *indexed.entry(target.clone()).or_insert(0) += count;
            }
        }
        if !active_by_demand.is_empty() {
            self.active_by_handle_demand.insert(id, active_by_demand);
        }
    }

    /// Retire every live target of one handle. Its declaration survives.
    pub(super) fn deactivate_handle(&mut self, id: HandleId) {
        let prior = self.active_by_handle_demand.remove(&id).unwrap_or_default();
        for (demand, targets) in prior {
            self.release(demand, targets);
        }
    }

    /// Retire only one handle's targets for one logical demand, leaving its
    /// siblings under other demands live.
    pub(super) fn deactivate_handle_demand(&mut self, id: HandleId, demand: DemandKey) {
        let Some(targets) = self
            .active_by_handle_demand
            .get_mut(&id)
            .and_then(|by_demand| by_demand.remove(&demand))
        else {
            return;
        };
        if self
            .active_by_handle_demand
            .get(&id)
            .is_some_and(BTreeMap::is_empty)
        {
            self.active_by_handle_demand.remove(&id);
        }
        self.release(demand, targets);
    }

    /// Forget every activation while keeping every declaration.
    ///
    /// The one operation a wholesale wire rebuild needs from this owner. It
    /// used to be two of that rebuild's twelve hand-written `.clear()` calls,
    /// which is exactly one map away from a silent half-reset.
    pub(super) fn forget_activations(&mut self) {
        self.by_demand.clear();
        self.active_by_handle_demand.clear();
    }

    fn release(&mut self, demand: DemandKey, targets: BTreeMap<RequestTarget, usize>) {
        for (reverse_target, count) in targets {
            let indexed = self
                .by_demand
                .get_mut(&demand)
                .expect("per-handle request-target demand mirrors the reverse index");
            let owned = indexed
                .get_mut(&reverse_target)
                .expect("per-handle request target mirrors the reverse index");
            *owned = owned
                .checked_sub(count)
                .expect("per-handle request-target refs mirror the reverse index");
            if *owned == 0 {
                indexed.remove(&reverse_target);
            }
            if indexed.is_empty() {
                self.by_demand.remove(&demand);
            }
        }
    }

    // -- reads --------------------------------------------------------------

    /// Every live execution target owned by any of `owner_demands`, plus what
    /// the walk had to examine to find them.
    pub(super) fn live_targets_for_demands(
        &self,
        owner_demands: &BTreeSet<DemandKey>,
    ) -> (Vec<(HandleId, String, u64)>, DemandWalk) {
        let mut walk = DemandWalk::default();
        let mut targets = BTreeSet::new();
        for demand in owner_demands {
            walk.demand_keys_touched += 1;
            let Some(indexed) = self.by_demand.get(demand) else {
                continue;
            };
            walk.candidates_examined += indexed.len() as u64;
            targets.extend(
                indexed
                    .keys()
                    .map(|target| (target.handle, target.path.clone(), target.revision)),
            );
        }
        (targets.into_iter().collect(), walk)
    }

    #[cfg(any(test, feature = "bench-instrumentation"))]
    pub(super) fn counts(&self) -> RequestTargetCounts {
        RequestTargetCounts {
            handles: self.by_handle.len(),
            demand_keys: self.by_demand.len(),
            edges: self.by_demand.values().map(BTreeMap::len).sum(),
            refs: self.by_demand.values().flat_map(BTreeMap::values).sum(),
            active_handles: self.active_by_handle_demand.len(),
            active_demand_keys: self
                .active_by_handle_demand
                .values()
                .map(BTreeMap::len)
                .sum(),
            active_edges: self
                .active_by_handle_demand
                .values()
                .flat_map(BTreeMap::values)
                .map(BTreeMap::len)
                .sum(),
            active_refs: self
                .active_by_handle_demand
                .values()
                .flat_map(BTreeMap::values)
                .flat_map(BTreeMap::values)
                .sum(),
        }
    }
}

/// The reads the execution-target proofs need, as questions rather than maps.
#[cfg(test)]
impl RequestTargets {
    pub(super) fn declared_for_handle(&self, id: HandleId) -> BTreeMap<ActiveRequestTarget, usize> {
        self.by_handle.get(&id).cloned().unwrap_or_default()
    }

    pub(super) fn declare_for_handle(
        &mut self,
        id: HandleId,
        target: ActiveRequestTarget,
        count: usize,
    ) {
        self.by_handle.entry(id).or_default().insert(target, count);
    }

    /// Every live target across every demand.
    pub(super) fn live_targets(&self) -> Vec<RequestTarget> {
        self.by_demand
            .values()
            .flat_map(BTreeMap::keys)
            .cloned()
            .collect()
    }

    /// Every branch with at least one live target.
    pub(super) fn live_handles(&self) -> BTreeSet<HandleId> {
        self.by_demand
            .values()
            .flat_map(BTreeMap::keys)
            .map(|target| target.handle)
            .collect()
    }

    pub(super) fn declared_live_for_demand(
        &self,
        demand: &DemandKey,
    ) -> BTreeMap<RequestTarget, usize> {
        self.by_demand.get(demand).cloned().unwrap_or_default()
    }

    pub(super) fn live_target_count(&self, demand: &DemandKey) -> usize {
        self.by_demand.get(demand).map_or(0, BTreeMap::len)
    }

    pub(super) fn has_live_demand(&self, demand: &DemandKey) -> bool {
        self.by_demand.contains_key(demand)
    }
}
