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
//! `RequestReplacements` (#1606) needed the identical shape with a
//! `RelaySessionKey` owner instead of a plan `SubId`, so the mechanism moved
//! to `owner_index.rs`, generic over the owner key. This file keeps its own
//! vocabulary — "plan", not "owner" — in the type alias and the trait impls
//! below; the generic module itself knows nothing about plans.
//!
//! ## Real ownership, not namespacing
//!
//! The three maps are private. `query.rs`, `auth_transport.rs`,
//! `mod.rs`, `coordinate_coverage.rs`, and `request_attempt.rs` used to
//! reach straight into them (`self.nip77.handoffs.insert(..)`,
//! `.sessions.iter()`, `.backfills.get(..)`); every one of those reach-ins is
//! now a method named for what the caller wanted -- a fact about repair
//! state, a relay-scoped teardown, a liveness sweep -- not the map it used
//! to touch. The plan's own lifecycle operations
//! ([`Nip77Sessions::cancel_repair_for_plan`],
//! [`Nip77Sessions::retire_backfills_for_plan`]) live here too: `EngineCore`
//! sequences the cross-owner consequences
//! (abandoning a wire id reaches into attempts, attribution, and other
//! owners this struct does not hold) from the [`PlanRepairWithdrawal`] this
//! owner hands back, but the decision of *which* subscriptions that
//! withdrawal touches is made here, once, from the owner's own state.
//!
//! ## What is not indexed here, deliberately
//!
//! A `BacklogActivatesLive` entry owns a NESTED live candidate that lives in
//! no map at all while its own EOSE is outstanding. That is a real asymmetry
//! in the protocol lifecycle, not an oversight, and this owner does not try to
//! index it — the teardown that must close it reads the typed value and
//! handles it explicitly.

use std::collections::{BTreeSet, HashMap};

use nmp_grammar::ConcreteFilter;
use nmp_router::SubId;
use nostr::{RelayUrl, Timestamp};

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
    handoffs: PlanIndexed<PendingNegHandoff>,
    /// Live reconciliation sessions keyed by their role-derived NIP-77 id.
    /// NIP-01 REQ ids and NIP-77 ids are separate namespaces by protocol and
    /// distinct values here, so closing one can never close the other.
    sessions: PlanIndexed<NegSession>,
    /// Every temporary NIP-01 request outside router demand: missing-id
    /// fetches and ordinary unlimited backlog fallbacks. The typed value
    /// determines the exact EOSE consequence; no boolean lifecycle flag.
    backfills: PlanIndexed<TemporaryReq>,
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

    #[cfg(test)]
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

    // -- children, by wire id ------------------------------------------------

    pub(super) fn insert_handoff(&mut self, sub_id: SubId, handoff: PendingNegHandoff) {
        self.handoffs.insert(sub_id, handoff);
    }

    pub(super) fn take_handoff(&mut self, sub_id: &SubId) -> Option<PendingNegHandoff> {
        self.handoffs.take(sub_id)
    }

    pub(super) fn is_pending_handoff(&self, sub_id: &SubId) -> bool {
        self.handoffs.contains(sub_id)
    }

    pub(super) fn insert_session(&mut self, sub_id: SubId, session: NegSession) {
        self.sessions.insert(sub_id, session);
    }

    pub(super) fn take_session(&mut self, sub_id: &SubId) -> Option<NegSession> {
        self.sessions.take(sub_id)
    }

    pub(super) fn get_session(&self, sub_id: &SubId) -> Option<&NegSession> {
        self.sessions.get(sub_id)
    }

    pub(super) fn get_session_mut(&mut self, sub_id: &SubId) -> Option<&mut NegSession> {
        self.sessions.get_mut(sub_id)
    }

    /// Every open reconciliation session on one relay, as the `(sub_id,
    /// filter)` pair a coordinate lookup needs — never the full `NegSession`,
    /// whose other fields (the reconciler, the attribution snapshot) are this
    /// owner's own business.
    pub(super) fn sessions_on_relay<'a>(
        &'a self,
        relay: &'a RelayUrl,
    ) -> impl Iterator<Item = (&'a SubId, &'a ConcreteFilter)> + 'a {
        self.sessions
            .iter()
            .filter(move |(_, session)| &session.relay == relay)
            .map(|(sub_id, session)| (sub_id, &session.filter))
    }

    pub(super) fn insert_backfill(&mut self, sub_id: SubId, request: TemporaryReq) {
        self.backfills.insert(sub_id, request);
    }

    pub(super) fn take_backfill(&mut self, sub_id: &SubId) -> Option<TemporaryReq> {
        self.backfills.take(sub_id)
    }

    #[cfg(test)]
    pub(super) fn is_pending_backfill(&self, sub_id: &SubId) -> bool {
        self.backfills.contains(sub_id)
    }

    /// Whether `sub_id` is the exact live candidate one plan's pending
    /// handoff currently owns -- what a retry decides currency against, not
    /// the raw index.
    pub(super) fn is_handoff_child_of(&self, plan: &SubId, sub_id: &SubId) -> bool {
        self.handoffs.children_of(plan).contains(sub_id)
    }

    /// Whether `sub_id` is one of one plan's live temporary backfill
    /// requests.
    pub(super) fn is_backfill_child_of(&self, plan: &SubId, sub_id: &SubId) -> bool {
        self.backfills.children_of(plan).contains(sub_id)
    }

    // -- liveness sweep -------------------------------------------------------
    //
    // `deadline_secs` is the coordinator's NIP-77 liveness policy
    // (`NEG_LIVENESS_DEADLINE_SECS`); this owner holds no timing constant of
    // its own, only the `started_at` each candidate was opened at.

    /// Every pending handoff whose live-candidate liveness deadline has
    /// already passed as of `now`, removed and handed back for its
    /// REQ-fallback teardown.
    pub(super) fn take_stale_handoffs(
        &mut self,
        now: Timestamp,
        deadline_secs: u64,
    ) -> Vec<(SubId, PendingNegHandoff)> {
        self.handoffs
            .take_where(|_, handoff| now >= handoff.started_at + deadline_secs)
    }

    /// Every reconciliation session whose liveness deadline has already
    /// passed as of `now`, removed and handed back for its REQ-fallback
    /// teardown.
    pub(super) fn take_stale_sessions(
        &mut self,
        now: Timestamp,
        deadline_secs: u64,
    ) -> Vec<(SubId, NegSession)> {
        self.sessions
            .take_where(|_, session| now >= session.started_at + deadline_secs)
    }

    /// The earliest liveness deadline across every open handoff and
    /// reconciliation session, or `None` if this owner holds neither --
    /// `next_deadline()`'s own NIP-77 term.
    pub(super) fn earliest_liveness_deadline(&self, deadline_secs: u64) -> Option<Timestamp> {
        self.sessions
            .iter()
            .map(|(_, session)| session.started_at + deadline_secs)
            .chain(
                self.handoffs
                    .iter()
                    .map(|(_, handoff)| handoff.started_at + deadline_secs),
            )
            .min()
    }

    // -- relay-scoped teardown -------------------------------------------------

    /// Every pending handoff whose candidate was probed on one relay,
    /// removed and handed back -- used when a fresh websocket generation
    /// makes a prior generation's candidates unreplayable.
    pub(super) fn take_handoffs_probed_on_relay(
        &mut self,
        relay: &RelayUrl,
    ) -> Vec<(SubId, PendingNegHandoff)> {
        self.handoffs
            .take_where(|_, handoff| handoff.probed.url() == relay)
    }

    /// Every pending handoff whose wire id names one relay, removed and
    /// handed back.
    pub(super) fn take_handoffs_on_relay(
        &mut self,
        relay: &RelayUrl,
    ) -> Vec<(SubId, PendingNegHandoff)> {
        self.handoffs.take_where(|sub_id, _| &sub_id.0 == relay)
    }

    /// Every reconciliation session open on one relay, removed and handed
    /// back.
    pub(super) fn take_sessions_on_relay(&mut self, relay: &RelayUrl) -> Vec<(SubId, NegSession)> {
        self.sessions
            .take_where(|_, session| &session.relay == relay)
    }

    /// Every temporary backfill request whose wire id names one relay,
    /// removed and handed back.
    pub(super) fn take_backfills_on_relay(
        &mut self,
        relay: &RelayUrl,
    ) -> Vec<(SubId, TemporaryReq)> {
        self.backfills.take_where(|sub_id, _| &sub_id.0 == relay)
    }

    // -- per-relay diagnostics -----------------------------------------------

    /// Every diagnostic relay-repair-phase question is a single yes/no over
    /// exactly one cluster; naming each avoids the caller reaching into three
    /// maps by hand to build its own state machine over them.
    pub(super) fn has_pending_handoff_on_relay(&self, relay: &RelayUrl) -> bool {
        self.handoffs.iter().any(|(sub_id, _)| &sub_id.0 == relay)
    }

    pub(super) fn has_reconciling_session_on_relay(&self, relay: &RelayUrl) -> bool {
        self.sessions
            .iter()
            .any(|(_, session)| &session.relay == relay)
    }

    pub(super) fn has_backlog_fallback_on_relay(&self, relay: &RelayUrl) -> bool {
        self.backfills.iter().any(|(sub_id, request)| {
            &sub_id.0 == relay
                && matches!(
                    request,
                    TemporaryReq::Backlog { .. } | TemporaryReq::BacklogActivatesLive { .. }
                )
        })
    }

    pub(super) fn has_missing_ids_backfill_on_relay(&self, relay: &RelayUrl) -> bool {
        self.backfills.iter().any(|(sub_id, request)| {
            &sub_id.0 == relay && matches!(request, TemporaryReq::MissingIds { .. })
        })
    }

    // -- the fan-out --------------------------------------------------------

    /// Every repair-state child indexed under one plan, across all three
    /// clusters, in one traversal.
    ///
    /// `role_sub_ids_for_plan` and the metadata fan-out each used to derive
    /// their own answer from three independent `children_of`/`get` pairs --
    /// one exhaustive with an `expect`, the other silently skipping a child
    /// its own index just reported (`else { continue }`). `children_of` and
    /// `get` can never actually disagree in production: every write goes
    /// through `insert`/`take`/`take_owner`, which keep the forward map and
    /// the reverse index in lockstep (`owner_index.rs`). Tolerating the
    /// disagreement anyway just hides the day that invariant breaks behind a
    /// silently smaller fan-out -- a plan's metadata update quietly missing
    /// one of its own roles. `expect` is the behavior this owner already
    /// chose for the same disagreement everywhere else (`OwnerIndexed::take`,
    /// `OwnerIndexed::take_owner`), so this is the one walk both derive from
    /// now, and the one strictness both inherit.
    pub(super) fn roles_for_plan(&self, plan_sub_id: &SubId) -> Vec<(SubId, PlanRole<'_>)> {
        let mut roles = Vec::new();
        for child in self.handoffs.children_of(plan_sub_id) {
            let handoff = self
                .handoffs
                .get(&child)
                .expect("a plan's handoff index names only live children");
            roles.push((child, PlanRole::Handoff(handoff)));
        }
        for child in self.sessions.children_of(plan_sub_id) {
            let session = self
                .sessions
                .get(&child)
                .expect("a plan's session index names only live children");
            roles.push((child, PlanRole::Session(session)));
        }
        for child in self.backfills.children_of(plan_sub_id) {
            let request = self
                .backfills
                .get(&child)
                .expect("a plan's backfill index names only live children");
            roles.push((child, PlanRole::Backfill(request)));
        }
        roles
    }

    /// Which role subscriptions one plan's metadata update applies to.
    ///
    /// The NIP-77 fan-out the request-attempt owner deliberately cannot see:
    /// a plan's live candidate, its reconciliation session, and its backlog
    /// children all carry the plan's claims. It lives here now, where that
    /// state lives — `request_attempt.rs` used to compute it by reaching into
    /// four of this owner's maps, with a comment promising exactly this move.
    pub(super) fn role_sub_ids_for_plan(&self, plan_sub_id: &SubId) -> BTreeSet<SubId> {
        let mut role_sub_ids = BTreeSet::from([plan_sub_id.clone()]);
        for (child, role) in self.roles_for_plan(plan_sub_id) {
            match role {
                PlanRole::Handoff(_) | PlanRole::Session(_) => {
                    role_sub_ids.insert(child);
                }
                // Exhaustive over the typed value. The `None => {}` arm this
                // replaced turned a broken mirror into a silently smaller
                // fan-out, which is the failure mode where a plan's metadata
                // update simply misses one of its own roles.
                PlanRole::Backfill(request) => match request {
                    // The ids-only fetch is not coverage proof and
                    // deliberately owns no plan claims. The retained NEG
                    // snapshot is extended separately by
                    // `extend_plan_execution_metadata`.
                    TemporaryReq::MissingIds { .. } => {}
                    TemporaryReq::Backlog { .. } => {
                        role_sub_ids.insert(child);
                    }
                    TemporaryReq::BacklogActivatesLive { live_sub_id, .. } => {
                        role_sub_ids.insert(child);
                        role_sub_ids.insert(live_sub_id.clone());
                    }
                },
            }
        }
        role_sub_ids
    }

    // -- plan repair withdrawal ----------------------------------------------

    /// Withdraw every pending/repair phase belonging to one plan -- pending
    /// handoffs, reconciliation sessions, and temporary backfills -- while
    /// deliberately leaving its currently-active live REQ alone (that is
    /// `Self::take_live`, the caller's own door).
    ///
    /// This owner cannot finish the withdrawal itself: releasing a wire id's
    /// cross-owner bookkeeping (attempts, attribution, pending request
    /// evidence, live wire requests) reaches into state this owner does not
    /// hold. What comes back is the exact set of consequences the caller
    /// still owes those other owners, plus how many children this withdrawal
    /// touched -- so a caller does not have to remember to count it.
    pub(super) fn cancel_repair_for_plan(&mut self, plan_sub_id: &SubId) -> PlanRepairWithdrawal {
        let mut withdrawal = PlanRepairWithdrawal::default();

        let pending = self.handoffs.take_owner(plan_sub_id);
        withdrawal.children_touched += pending.len() as u64;
        withdrawal
            .abandon_and_close
            .extend(pending.into_iter().map(|(id, _)| id));

        let neg_ids = self.sessions.take_owner(plan_sub_id);
        withdrawal.children_touched += neg_ids.len() as u64;
        withdrawal
            .neg_closes
            .extend(neg_ids.into_iter().map(|(id, session)| (id, session.relay)));

        withdrawal.extend(self.retire_backfills_for_plan(plan_sub_id));
        withdrawal
    }

    /// Withdraw every temporary backfill request owned by one plan. Shared by
    /// [`Self::cancel_repair_for_plan`] and the coordinator's backlog-start
    /// path, because a `BacklogActivatesLive` entry owns a NESTED live
    /// candidate (and sometimes its predecessor) that lives in no other map
    /// at all -- a second, hand-rolled teardown that forgot that nesting
    /// would leak the candidate on the wire forever and leave a wire id a
    /// late EOSE could still resolve through.
    pub(super) fn retire_backfills_for_plan(
        &mut self,
        plan_sub_id: &SubId,
    ) -> PlanRepairWithdrawal {
        let mut withdrawal = PlanRepairWithdrawal::default();
        let temporary = self.backfills.take_owner(plan_sub_id);
        withdrawal.children_touched += temporary.len() as u64;
        for (sub_id, request) in temporary {
            match &request {
                TemporaryReq::MissingIds { neg_sub_id, .. } => {
                    // NEG has already closed on the wire, but its coverage
                    // snapshot intentionally remained alive while the missing
                    // ids were in flight. Withdrawing/superseding that fetch
                    // must release the deferred snapshot too -- without
                    // closing it a second time.
                    withdrawal.abandon_only.push(neg_sub_id.clone());
                }
                TemporaryReq::BacklogActivatesLive {
                    live_sub_id,
                    prior_live_sub_id,
                    ..
                } => {
                    // The live candidate REQ is tracked ONLY inside this
                    // fallback entry while its own EOSE is still
                    // outstanding -- it lives in neither `handoffs` nor the
                    // live map. Withdrawing/superseding demand mid-fallback
                    // must close and discard it here, or it leaks forever: a
                    // late EOSE on its orphaned wire id would otherwise still
                    // resolve through attribution and mint phantom coverage
                    // for demand that no longer exists.
                    withdrawal.abandon_and_close.push(live_sub_id.clone());
                    // `prior_live_sub_id` is ordinarily still this plan's
                    // active live entry, closed either by the caller's own
                    // full withdrawal or carried forward into the next
                    // handoff's own `prior_live_sub_id`. Only close it here
                    // if it has already drifted away from that slot, so this
                    // never double-closes a subscription another path owns.
                    if let Some(prior) = prior_live_sub_id {
                        if self.live_for_plan(plan_sub_id) != Some(prior) {
                            withdrawal.abandon_and_close.push(prior.clone());
                        }
                    }
                }
                TemporaryReq::Backlog { .. } => {}
            }
            withdrawal.abandon_and_close.push(sub_id);
        }
        withdrawal
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

    /// Test-only: swap which plan's reverse set names which plan's pending
    /// handoff, corrupting `by_owner` alone -- the forward map and every
    /// count `counts()` reports are unchanged. The cardinality-preserving
    /// corruption `assert_consistent` exists to catch, exposed here only so
    /// a falsifier can drive it without reaching past this owner's own
    /// fields (which are private even to `EngineCore`).
    #[cfg(test)]
    pub(super) fn swap_handoff_owners_for_test(&mut self, a: &SubId, b: &SubId) {
        self.handoffs.swap_owners_for_test(a, b);
    }

    // -- test-only reads ------------------------------------------------------
    //
    // What the plan-metadata proofs need to see into one cluster without a
    // raw field, mirroring `live_is_empty` above.

    #[cfg(test)]
    pub(super) fn handoffs_is_empty(&self) -> bool {
        self.handoffs.is_empty()
    }

    #[cfg(test)]
    pub(super) fn sessions_is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    #[cfg(test)]
    pub(super) fn backfills_is_empty(&self) -> bool {
        self.backfills.is_empty()
    }

    #[cfg(test)]
    pub(super) fn handoff_children_of(&self, plan: &SubId) -> BTreeSet<SubId> {
        self.handoffs.children_of(plan)
    }

    #[cfg(test)]
    pub(super) fn session_children_of(&self, plan: &SubId) -> BTreeSet<SubId> {
        self.sessions.children_of(plan)
    }

    #[cfg(test)]
    pub(super) fn backfill_children_of(&self, plan: &SubId) -> BTreeSet<SubId> {
        self.backfills.children_of(plan)
    }
}

/// One repair-state child indexed under a plan, wrapping which cluster it
/// belongs to instead of naming the map it came from.
pub(super) enum PlanRole<'a> {
    Handoff(&'a PendingNegHandoff),
    Session(&'a NegSession),
    Backfill(&'a TemporaryReq),
}

/// Every consequence of withdrawing one plan's repair state that reaches
/// outside this owner: which wire ids the caller must abandon (release
/// attempts/attribution/pending-request-evidence/live-wire-request
/// bookkeeping this owner does not hold), which of those must also close on
/// the wire, which relays need an explicit NEG-CLOSE effect, and how many
/// children this withdrawal touched -- so no caller has to remember to add
/// that count up itself.
#[derive(Default)]
pub(super) struct PlanRepairWithdrawal {
    /// Abandon, then close on the wire.
    pub(super) abandon_and_close: Vec<SubId>,
    /// Abandon without a wire close -- already closed by the relay's own
    /// EOSE protocol (a completed NEG's deferred missing-ids snapshot).
    pub(super) abandon_only: Vec<SubId>,
    /// Reconciliation sessions needing an explicit NEG-CLOSE effect, paired
    /// with the relay it targets.
    pub(super) neg_closes: Vec<(SubId, RelayUrl)>,
    /// How many children this withdrawal touched, across every cluster it
    /// drew from.
    pub(super) children_touched: u64,
}

impl PlanRepairWithdrawal {
    fn extend(&mut self, other: Self) {
        self.abandon_and_close.extend(other.abandon_and_close);
        self.abandon_only.extend(other.abandon_only);
        self.neg_closes.extend(other.neg_closes);
        self.children_touched += other.children_touched;
    }
}
