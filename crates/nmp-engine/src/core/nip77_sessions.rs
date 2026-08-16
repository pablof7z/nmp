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
//! They are one [`PlanIndexed`] now — a NIP-77-flavored name for
//! [`OwnerIndexed`](super::owner_index::OwnerIndexed) keyed by plan `SubId` on
//! both sides. It is also where the *other* removal direction lives:
//! plan-scoped teardown used to `remove` the reverse-index entry and then loop
//! removing children from the forward map, open-coded at three more sites.
//! Both directions are one implementation, so the two maps cannot disagree
//! about who owns whom.
//!
//! `RequestReplacements` (#1562) needed the identical shape with a
//! `RelaySessionKey` owner instead of a plan `SubId`, so the mechanism moved
//! to `owner_index.rs`, generic over the owner key. This file keeps its own
//! vocabulary — "plan", not "owner" — in the type alias and the trait impls
//! below; the generic module itself knows nothing about plans.
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

use super::owner_index::{IndexedChild, OwnerIndexed};
use super::{NegSession, PendingNegHandoff, TemporaryReq};

/// NIP-77's own name for the generic mirrored index, keyed by plan `SubId` on
/// both the child and the owner side.
pub(super) type PlanIndexed<V> = OwnerIndexed<SubId, SubId, V>;

impl IndexedChild<SubId> for PendingNegHandoff {
    fn owner_key(&self) -> &SubId {
        &self.plan_sub_id
    }
}

impl IndexedChild<SubId> for NegSession {
    fn owner_key(&self) -> &SubId {
        &self.plan_sub_id
    }
}

impl IndexedChild<SubId> for TemporaryReq {
    fn owner_key(&self) -> &SubId {
        TemporaryReq::plan_sub_id(self)
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

impl Default for Nip77Sessions {
    /// `PlanIndexed` needs an owner label for its panic text (`what`), which
    /// `#[derive(Default)]` cannot supply. Three distinct labels here, rather
    /// than one shared "NIP-77" label, so a broken mirror in the handoff
    /// index and a broken mirror in the reconciliation index read as
    /// different failures.
    fn default() -> Self {
        Self {
            handoffs: PlanIndexed::new("NIP-77 handoff"),
            sessions: PlanIndexed::new("NIP-77 reconciliation"),
            backfills: PlanIndexed::new("NIP-77 backfill"),
            live: HashMap::new(),
            next_incarnation: 0,
        }
    }
}

impl Nip77Sessions {
    /// Mint the next role incarnation. The only way this counter moves.
    pub(super) fn mint_incarnation(&mut self) -> u64 {
        let incarnation = self.next_incarnation;
        // Checked, not wrapping. Exhausting a u64 at one mint per repair phase
        // is not reachable by this process -- which is an argument for the
        // width, not for silently re-minting a string a straggler EOSE could
        // still be addressed to if it ever were.
        self.next_incarnation = self
            .next_incarnation
            .checked_add(1)
            .expect("NIP-77 role incarnations are exhausted; ids must never be reused");
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
            // Exhaustive over the typed value. The `None => {}` arm this
            // replaced turned a broken mirror into a silently smaller fan-out,
            // which is the failure mode where a plan's metadata update simply
            // misses one of its own roles.
            let request = self
                .backfills
                .get(&child)
                .expect("a plan's backfill index names only live children");
            match request {
                // The ids-only fetch is not coverage proof and deliberately
                // owns no plan claims. The retained NEG snapshot is extended
                // separately by `extend_plan_execution_metadata`.
                TemporaryReq::MissingIds { .. } => {}
                TemporaryReq::Backlog { .. } => {
                    role_sub_ids.insert(child);
                }
                TemporaryReq::BacklogActivatesLive { live_sub_id, .. } => {
                    let live_sub_id = live_sub_id.clone();
                    role_sub_ids.insert(child);
                    role_sub_ids.insert(live_sub_id);
                }
            }
        }
        role_sub_ids
    }

    #[cfg(any(test, feature = "bench-instrumentation"))]
    pub(super) fn assert_consistent(&self, at: &str) {
        self.handoffs.assert_consistent(at);
        self.sessions.assert_consistent(at);
        self.backfills.assert_consistent(at);
    }

    #[cfg(any(test, feature = "bench-instrumentation"))]
    pub(super) fn counts(&self) -> Nip77Counts {
        Nip77Counts {
            live: self.live.len(),
            handoffs: self.handoffs.len(),
            handoff_plan_keys: self.handoffs.owner_keys(),
            handoff_plan_edges: self.handoffs.owner_edges(),
            sessions: self.sessions.len(),
            session_plan_keys: self.sessions.owner_keys(),
            session_plan_edges: self.sessions.owner_edges(),
            backfills: self.backfills.len(),
            backfill_plan_keys: self.backfills.owner_keys(),
            backfill_plan_edges: self.backfills.owner_edges(),
        }
    }
}
