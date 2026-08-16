//! Typed ownership for NIP-77 repair state: every child subscription a router
//! plan currently owns, plus which live REQ is serving that plan's tail.
//!
//! ## One index, written once
//!
//! Three separate clusters here — pending handoffs, reconciliation sessions,
//! and temporary backlog requests — have the identical shape: a map of
//! children keyed by their own wire id, and a reverse index from the plan that
//! owns them. Each had a hand-written insert and a hand-written take, six
//! functions whose bodies were verbatim copies differing only in the value
//! type and how the plan id was read off it. A fourth cluster would have been
//! a seventh and eighth copy.
//!
//! They are one [`PlanIndexed`] now. It is also where the *other* removal
//! direction lives: plan-scoped teardown used to `remove` the reverse-index
//! entry and then loop removing children from the forward map, open-coded at
//! three more sites. Both directions are one implementation, so the two maps
//! cannot disagree about who owns whom.
//!
//! ## What is not indexed here, deliberately
//!
//! A `BacklogActivatesLive` entry owns a NESTED live candidate that lives in
//! no map at all while its own EOSE is outstanding. That is a real asymmetry
//! in the protocol lifecycle, not an oversight, and this owner does not try to
//! index it — the teardown that must close it reads the typed value and
//! handles it explicitly.

use std::collections::{BTreeSet, HashMap};

use nmp_router::SubId;
use nostr::RelayUrl;

use super::{NegSession, PendingNegHandoff, TemporaryReq};

/// A subscription that exists on behalf of exactly one router plan.
pub(super) trait PlanChild {
    fn plan_sub_id(&self) -> &SubId;
}

impl PlanChild for PendingNegHandoff {
    fn plan_sub_id(&self) -> &SubId {
        &self.plan_sub_id
    }
}

impl PlanChild for NegSession {
    fn plan_sub_id(&self) -> &SubId {
        &self.plan_sub_id
    }
}

impl PlanChild for TemporaryReq {
    fn plan_sub_id(&self) -> &SubId {
        TemporaryReq::plan_sub_id(self)
    }
}

/// Children keyed by their own wire id, with the reverse index from the plan
/// that owns them maintained as a consequence rather than by every caller.
///
/// Both maps are private. There is no spelling of "insert into one and forget
/// the other", in either removal direction.
pub(super) struct PlanIndexed<V: PlanChild> {
    by_child: HashMap<SubId, V>,
    by_plan: HashMap<SubId, BTreeSet<SubId>>,
}

impl<V: PlanChild> Default for PlanIndexed<V> {
    fn default() -> Self {
        Self {
            by_child: HashMap::new(),
            by_plan: HashMap::new(),
        }
    }
}

impl<V: PlanChild> PlanIndexed<V> {
    pub(super) fn insert(&mut self, sub_id: SubId, value: V) {
        self.by_plan
            .entry(value.plan_sub_id().clone())
            .or_default()
            .insert(sub_id.clone());
        self.by_child.insert(sub_id, value);
    }

    /// Remove one child and prune its plan's set.
    pub(super) fn take(&mut self, sub_id: &SubId) -> Option<V> {
        let value = self.by_child.remove(sub_id)?;
        let plan = value.plan_sub_id().clone();
        if let Some(children) = self.by_plan.get_mut(&plan) {
            children.remove(sub_id);
            if children.is_empty() {
                self.by_plan.remove(&plan);
            }
        }
        Some(value)
    }

    /// Remove every child of one plan. The returned order is the reverse
    /// index's own, which is stable because `by_plan`'s sets are ordered.
    pub(super) fn take_plan(&mut self, plan: &SubId) -> Vec<(SubId, V)> {
        let children = self.by_plan.remove(plan).unwrap_or_default();
        children
            .into_iter()
            .filter_map(|child| {
                let value = self.by_child.remove(&child)?;
                Some((child, value))
            })
            .collect()
    }

    /// Remove every child matching `drop`, pruning the reverse index for each,
    /// and hand back what was removed.
    ///
    /// Five sites used to write this as "collect the matching ids, then loop
    /// calling take" — and, having thrown the values away in the collect,
    /// three of them looked each one up a second time to act on it.
    pub(super) fn take_where<F: Fn(&SubId, &V) -> bool>(&mut self, drop: F) -> Vec<(SubId, V)> {
        let departing: Vec<_> = self
            .by_child
            .iter()
            .filter(|(child, value)| drop(child, value))
            .map(|(child, _)| child.clone())
            .collect();
        departing
            .into_iter()
            .filter_map(|child| self.take(&child).map(|value| (child, value)))
            .collect()
    }

    pub(super) fn get(&self, sub_id: &SubId) -> Option<&V> {
        self.by_child.get(sub_id)
    }

    pub(super) fn get_mut(&mut self, sub_id: &SubId) -> Option<&mut V> {
        self.by_child.get_mut(sub_id)
    }

    pub(super) fn contains(&self, sub_id: &SubId) -> bool {
        self.by_child.contains_key(sub_id)
    }

    pub(super) fn children_of(&self, plan: &SubId) -> BTreeSet<SubId> {
        self.by_plan.get(plan).cloned().unwrap_or_default()
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = (&SubId, &V)> {
        self.by_child.iter()
    }

    #[cfg(any(test, feature = "bench-instrumentation"))]
    pub(super) fn len(&self) -> usize {
        self.by_child.len()
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.by_child.is_empty() && self.by_plan.is_empty()
    }

    #[cfg(any(test, feature = "bench-instrumentation"))]
    pub(super) fn plan_keys(&self) -> usize {
        self.by_plan.len()
    }

    #[cfg(any(test, feature = "bench-instrumentation"))]
    pub(super) fn plan_edges(&self) -> usize {
        self.by_plan.values().map(BTreeSet::len).sum()
    }
}

/// The census contribution, so the root counts this owner's state without
/// naming its maps.
#[cfg(any(test, feature = "bench-instrumentation"))]
pub(super) struct Nip77Counts {
    pub(super) live: usize,
    pub(super) handoffs: usize,
    pub(super) handoff_plan_keys: usize,
    pub(super) handoff_plan_edges: usize,
    pub(super) sessions: usize,
    pub(super) session_plan_keys: usize,
    pub(super) session_plan_edges: usize,
    pub(super) backfills: usize,
    pub(super) backfill_plan_keys: usize,
    pub(super) backfill_plan_edges: usize,
}

#[derive(Default)]
pub(super) struct Nip77Sessions {
    /// Candidate live REQs waiting for their exact EOSE barrier.
    pub(super) handoffs: PlanIndexed<PendingNegHandoff>,
    /// Live reconciliation sessions keyed by their role-derived NIP-77 id.
    /// NIP-01 REQ ids and NIP-77 ids are separate namespaces by protocol and
    /// distinct values here, so closing one can never close the other.
    pub(super) sessions: PlanIndexed<NegSession>,
    /// Every temporary NIP-01 request outside router demand: missing-id
    /// fetches and ordinary unlimited backlog fallbacks. The typed value
    /// determines the exact EOSE consequence; no boolean lifecycle flag.
    pub(super) backfills: PlanIndexed<TemporaryReq>,
    /// Router plan id -> exact NIP-01 subscription currently owning the live
    /// tail. NIP-77 candidates use role-derived ids, so an old live selection
    /// can overlap a replacement until the replacement's EOSE.
    live: HashMap<SubId, SubId>,
    /// Monotonic reincarnation counter for every NIP-77 role wire id
    /// ([`nip77_role_sub_id`], #932). ONLY ever increments: it survives
    /// recompiles, `AttributionState::clear_session`, and reconnects
    /// untouched, because a counter that reset would re-mint a string a
    /// straggler EOSE could still be addressed to — exactly the defect it
    /// exists to close. `u64` at one mint per repair phase is not a
    /// wrap-around this process can reach.
    ///
    /// Private with no setter, so "only ever increments" is a property of
    /// this file rather than a request in a doc comment.
    next_incarnation: u64,
}

impl Nip77Sessions {
    /// Mint the next role incarnation. The only way this counter moves.
    pub(super) fn mint_incarnation(&mut self) -> u64 {
        let incarnation = self.next_incarnation;
        self.next_incarnation = self.next_incarnation.wrapping_add(1);
        incarnation
    }

    // -- the live tail ------------------------------------------------------

    pub(super) fn live_for_plan(&self, plan: &SubId) -> Option<&SubId> {
        self.live.get(plan)
    }

    pub(super) fn has_live(&self, plan: &SubId) -> bool {
        self.live.contains_key(plan)
    }

    pub(super) fn set_live(&mut self, plan: SubId, live_sub_id: SubId) {
        self.live.insert(plan, live_sub_id);
    }

    pub(super) fn take_live(&mut self, plan: &SubId) -> Option<SubId> {
        self.live.remove(plan)
    }

    /// Forget every plan whose live tail was being served by one relay.
    pub(super) fn drop_live_for_relay(&mut self, relay: &RelayUrl) {
        self.live.retain(|plan_sub_id, _| &plan_sub_id.0 != relay);
    }

    #[cfg(test)]
    pub(super) fn live_is_empty(&self) -> bool {
        self.live.is_empty()
    }

    pub(super) fn has_live_on_relay(&self, relay: &RelayUrl) -> bool {
        self.live.keys().any(|plan_sub_id| &plan_sub_id.0 == relay)
    }

    /// Whether one plan owns any repair state at all — a live tail, a pending
    /// handoff, a reconciliation, or a temporary request. Asking this used to
    /// mean four separate `contains_key` calls against four maps, which is
    /// four chances to add a fifth map and not extend the question.
    pub(super) fn has_repair_state(&self, plan: &SubId) -> bool {
        self.live.contains_key(plan)
            || !self.handoffs.children_of(plan).is_empty()
            || !self.sessions.children_of(plan).is_empty()
            || !self.backfills.children_of(plan).is_empty()
    }

    // -- the fan-out --------------------------------------------------------

    /// Which role subscriptions one plan's metadata update applies to.
    ///
    /// The NIP-77 fan-out the request-attempt owner deliberately cannot see:
    /// a plan's live candidate, its reconciliation session, and its backlog
    /// children all carry the plan's claims. It lives here now, where that
    /// state lives — `request_attempt.rs` used to compute it by reaching into
    /// four of this owner's maps, with a comment promising exactly this move.
    pub(super) fn role_sub_ids_for_plan(&self, plan_sub_id: &SubId) -> BTreeSet<SubId> {
        let mut role_sub_ids = BTreeSet::from([plan_sub_id.clone()]);
        role_sub_ids.extend(self.handoffs.children_of(plan_sub_id));
        role_sub_ids.extend(self.sessions.children_of(plan_sub_id));
        for child in self.backfills.children_of(plan_sub_id) {
            match self.backfills.get(&child) {
                // The ids-only fetch is not coverage proof and deliberately
                // owns no plan claims. The retained NEG snapshot is extended
                // separately by `extend_plan_execution_metadata`.
                Some(TemporaryReq::MissingIds { .. }) => {}
                Some(TemporaryReq::Backlog { .. }) => {
                    role_sub_ids.insert(child);
                }
                Some(TemporaryReq::BacklogActivatesLive { live_sub_id, .. }) => {
                    let live_sub_id = live_sub_id.clone();
                    role_sub_ids.insert(child);
                    role_sub_ids.insert(live_sub_id);
                }
                None => {}
            }
        }
        role_sub_ids
    }

    #[cfg(any(test, feature = "bench-instrumentation"))]
    pub(super) fn counts(&self) -> Nip77Counts {
        Nip77Counts {
            live: self.live.len(),
            handoffs: self.handoffs.len(),
            handoff_plan_keys: self.handoffs.plan_keys(),
            handoff_plan_edges: self.handoffs.plan_edges(),
            sessions: self.sessions.len(),
            session_plan_keys: self.sessions.plan_keys(),
            session_plan_edges: self.sessions.plan_edges(),
            backfills: self.backfills.len(),
            backfill_plan_keys: self.backfills.plan_keys(),
            backfill_plan_edges: self.backfills.plan_edges(),
        }
    }
}
