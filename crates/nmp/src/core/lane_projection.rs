//! The reducer-owned lane-worker projection (issue #985).
//!
//! Relay-worker demand for the write plane is exactly "which relay sessions
//! does a still-open durable lane need?". Before this module the reducer
//! answered that question by asking the durable store, once per pending
//! intent, on **every** effect-dispatch pass: a redb prefix range scan, a
//! UTF-8 key comparison per row, a `serde_json` decode of every
//! `RecoveredLane`, and a `RelayUrl` reparse. A production Mosaico daemon
//! profile attributed 67.5% of the pegged engine thread to
//! `RedbStore::recover_outbox_lanes` reached this way. The durable rows never
//! changed between those passes; the reducer was reconstructing state it had
//! just committed itself.
//!
//! So the reducer keeps the answer. `PendingWrite::nonterminal_lane_relays`
//! is an exact projection of the committed lane states — a relay is a member
//! iff its latest committed lane for that intent is nonterminal — and
//! [`EngineCore::write_relay_workers`] becomes a pure in-memory union with
//! zero store reads, zero JSON decodes, and zero URL parses.
//!
//! ## Why this file exists at all
//!
//! A projection maintained by "remember to also update the set" at ~25 call
//! sites is a bug waiting for its next contributor. Every engine-owned lane
//! mutation therefore goes through exactly one door here. Each door wraps its
//! `EventStore` counterpart, consumes the exact post-commit [`RecoveredLane`]
//! the store already returns, and feeds it to
//! [`EngineCore::project_committed_lane`]. Nothing in `crates/nmp/src` may
//! call `store_mut().set_lane_*` / `start_lane_attempt` /
//! `record_lane_handoff` / `finish_lane_attempt` / `suspend_lane_attempt` /
//! `bootstrap_outbox_lanes` / `record_route_revision` /
//! `close_terminal_intent` directly, and
//! `every_lane_mutation_constructor_goes_through_the_projection_door` (in
//! `lane_projection_tests.rs`) fails mechanically if it does.
//!
//! The route-revision door is in that enumeration deliberately even though a
//! revision mints no lane by itself: under #975 `Auto` re-executes its
//! strategy at every send opportunity and appends a revision whenever
//! resolution learns something new, and each revision mints lanes through the
//! bootstrap door this projection depends on. An enumeration written only
//! against today's two call sites would be silently incomplete the moment
//! #975 lands.
//!
//! ## Failure and durability
//!
//! Removal from the projection happens **only** on a committed terminal
//! `RecoveredLane`. A failed mutation is classified by #904's
//! [`DurabilityOutcome`]:
//!
//! - `Absent` — the transition provably did not land. The projection is not
//!   touched; the pre-transition state is still exactly true.
//! - `Unknown` — the transition may already be durable. The projection
//!   conservatively **adds** every relay the attempted mutation could have
//!   created or kept alive and latches `EngineCore::worker_projection_
//!   degraded`. The set may then over-retain a worker; it can never drop one
//!   that a possibly-committed lane needs.
//!
//! Degradation is never a licence to go back to scanning: a per-dispatch
//! fallback scan would reinstate the exact defect this projection exists to
//! remove. Reconciliation is the explicit, one-shot rebuild
//! `recover_on_boot` already performs from canonical rows after a reopen.

use super::*;

/// The enumerated contract surface for the doors below. Test-only: it exists
/// so `every_lane_mutation_constructor_goes_through_the_projection_door` can
/// assert BOTH that the enumeration still covers every `EventStore`
/// lane-mutation door and that no engine source file reaches one of them
/// around the projection.
#[cfg(test)]
pub(super) struct LaneProjection;

#[cfg(test)]
impl LaneProjection {
    /// The enumerated lane-mutation constructors. Adding an `EventStore`
    /// door that mints or transitions a lane means adding it here **and**
    /// giving it a projection door below; the source-level test enforces the
    /// second half.
    pub(super) const LANE_MUTATION_DOORS: &'static [&'static str] = &[
        "bootstrap_outbox_lanes",
        "record_route_revision",
        "set_lane_waiting",
        "set_lane_eligible",
        "set_lane_transient",
        "suspend_lane_attempt",
        "start_lane_attempt",
        "record_lane_handoff",
        "finish_lane_attempt",
        "close_terminal_intent",
    ];
}

impl<S: EventStore> EngineCore<S> {
    /// Apply one exact post-commit lane fact to the reducer's projections.
    ///
    /// This is the ONLY place `nonterminal_lane_relays` grows or shrinks on a
    /// committed fact, and it also subsumes what the two `bootstrap_outbox_
    /// lanes` call sites used to do by hand for `lane_relays` /
    /// `receipts_by_lane_relay` (epic #507 finding E5's reverse wake index).
    /// `lane_relays` still means "every lane ever learned", including
    /// terminal ones — `close_terminal_intent` deliberately retains terminal
    /// lane rows, and `forget_pending_indexes` walks that set to clean the
    /// reverse index.
    pub(super) fn project_committed_lane(&mut self, lane: &RecoveredLane) {
        let Some(id) = self.receipt_for_intent(lane.key.intent_id) else {
            return;
        };
        let Some(pending) = self.pending.get_mut(&id) else {
            return;
        };
        if pending.lane_relays.insert(lane.key.relay.clone()) {
            self.receipts_by_lane_relay
                .entry(lane.key.relay.clone())
                .or_default()
                .insert(id);
        }
        if matches!(lane.state, LaneState::Terminal { .. }) {
            pending.nonterminal_lane_relays.remove(&lane.key.relay);
        } else {
            pending
                .nonterminal_lane_relays
                .insert(lane.key.relay.clone());
        }
    }

    /// The fail-closed half of [`Self::project_committed_lane`]. `candidates`
    /// is every relay the attempted mutation could have created or left
    /// nonterminal.
    fn project_failed_lane_mutation(
        &mut self,
        intent_id: IntentId,
        candidates: impl IntoIterator<Item = RelayUrl>,
        error: &PersistenceError,
    ) {
        if error.durability() == DurabilityOutcome::Absent {
            // Provably nothing landed. The projection already describes the
            // durable truth exactly; touching it would make it wrong.
            return;
        }
        // May be absent, may already be durable (#904). Over-retain.
        self.worker_projection_degraded = true;
        let Some(id) = self.receipt_for_intent(intent_id) else {
            return;
        };
        for relay in candidates {
            let Some(pending) = self.pending.get_mut(&id) else {
                return;
            };
            if pending.lane_relays.insert(relay.clone()) {
                self.receipts_by_lane_relay
                    .entry(relay.clone())
                    .or_default()
                    .insert(id);
            }
            pending.nonterminal_lane_relays.insert(relay);
        }
    }

    /// Every relay this intent currently owns a lane on, as the conservative
    /// candidate set for a mutation whose durability is unknown.
    fn lane_candidates(&self, intent_id: IntentId) -> Vec<RelayUrl> {
        self.receipt_for_intent(intent_id)
            .and_then(|id| self.pending.get(&id))
            .map(|pending| pending.lane_relays.iter().cloned().collect())
            .unwrap_or_default()
    }

    // ---- the enumerated lane-mutation doors -----------------------------
    //
    // Each one is the store door plus exactly one projection update. They
    // return what the store door returns, so a call site's error handling is
    // unchanged.

    /// Idempotently seed lanes for `intent_id` and learn all of them.
    /// `candidates` is the route set this bootstrap could mint lanes for —
    /// used only when the failure's durability is `Unknown`.
    pub(super) fn commit_lane_bootstrap(
        &mut self,
        intent_id: IntentId,
        candidates: &BTreeSet<RelayUrl>,
    ) -> Result<Vec<RecoveredLane>, PersistenceError> {
        match self.resolver.store_mut().bootstrap_outbox_lanes(intent_id) {
            Ok(lanes) => {
                for lane in &lanes {
                    self.project_committed_lane(lane);
                }
                Ok(lanes)
            }
            Err(error) => {
                self.project_failed_lane_mutation(intent_id, candidates.iter().cloned(), &error);
                Err(error)
            }
        }
    }

    /// Append the next canonical resolved-route revision. A revision mints no
    /// lane by itself — the paired `commit_lane_bootstrap` does — but it is a
    /// lane-minting constructor under #975, so it owns a door and is
    /// enumerated. On unknown durability its relays become projection
    /// candidates: the revision may be durable, so a later bootstrap (this
    /// process or the next) can mint exactly those lanes.
    pub(super) fn commit_route_revision(
        &mut self,
        intent_id: IntentId,
        relays: BTreeSet<RelayUrl>,
    ) -> Result<RecoveredRouteRevision, PersistenceError> {
        match self
            .resolver
            .store_mut()
            .record_route_revision(intent_id, relays.clone())
        {
            Ok(revision) => Ok(revision),
            Err(error) => {
                self.project_failed_lane_mutation(intent_id, relays, &error);
                Err(error)
            }
        }
    }

    pub(super) fn commit_lane_waiting(
        &mut self,
        key: &LaneKey,
        expected_revision: u64,
        auth: bool,
    ) -> Result<RecoveredLane, PersistenceError> {
        let result = self
            .resolver
            .store_mut()
            .set_lane_waiting(key, expected_revision, auth);
        self.absorb_lane_result(key, result)
    }

    pub(super) fn commit_lane_eligible(
        &mut self,
        key: &LaneKey,
        expected_revision: u64,
        since: Timestamp,
    ) -> Result<RecoveredLane, PersistenceError> {
        let result = self
            .resolver
            .store_mut()
            .set_lane_eligible(key, expected_revision, since);
        self.absorb_lane_result(key, result)
    }

    pub(super) fn commit_lane_transient(
        &mut self,
        key: &LaneKey,
        expected_revision: u64,
        ordinal: u64,
        eligible_at: Timestamp,
        cause: TransientCause,
        raw_reason: Option<String>,
    ) -> Result<RecoveredLane, PersistenceError> {
        let result = self.resolver.store_mut().set_lane_transient(
            key,
            expected_revision,
            ordinal,
            eligible_at,
            cause,
            raw_reason,
        );
        self.absorb_lane_result(key, result)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn commit_lane_suspend(
        &mut self,
        key: &LaneKey,
        expected_revision: u64,
        ordinal: u64,
        at: Timestamp,
        cause: TransientCause,
        raw_reason: Option<String>,
        auth: bool,
    ) -> Result<RecoveredLane, PersistenceError> {
        let result = self.resolver.store_mut().suspend_lane_attempt(
            key,
            expected_revision,
            ordinal,
            at,
            cause,
            raw_reason,
            auth,
        );
        self.absorb_lane_result(key, result)
    }

    pub(super) fn commit_lane_attempt_start(
        &mut self,
        key: &LaneKey,
        expected_revision: u64,
        event: SignedEvent,
        started_at: Timestamp,
    ) -> Result<(RecoveredAttempt, RecoveredLane), PersistenceError> {
        match self.resolver.store_mut().start_lane_attempt(
            key,
            expected_revision,
            event,
            started_at,
        ) {
            Ok((attempt, lane)) => {
                self.project_committed_lane(&lane);
                Ok((attempt, lane))
            }
            Err(error) => {
                self.project_failed_lane_mutation(key.intent_id, [key.relay.clone()], &error);
                Err(error)
            }
        }
    }

    pub(super) fn commit_lane_handoff(
        &mut self,
        key: &LaneKey,
        expected_revision: u64,
        ordinal: u64,
        detail: AttemptHandoffDetail,
        next: PostHandoffState,
    ) -> Result<RecoveredLane, PersistenceError> {
        let result = self.resolver.store_mut().record_lane_handoff(
            key,
            expected_revision,
            ordinal,
            detail,
            next,
        );
        self.absorb_lane_result(key, result)
    }

    pub(super) fn commit_lane_finish(
        &mut self,
        key: &LaneKey,
        expected_revision: u64,
        ordinal: u64,
        outcome: AttemptOutcome,
        finished_at: Timestamp,
    ) -> Result<RecoveredLane, PersistenceError> {
        let result = self.resolver.store_mut().finish_lane_attempt(
            key,
            expected_revision,
            ordinal,
            outcome,
            finished_at,
        );
        self.absorb_lane_result(key, result)
    }

    /// The store validates the complete all-terminal invariant inside its own
    /// transaction, so the projection only decides that an attempt is
    /// plausible — it is never the authority for closure. On unknown
    /// durability every lane this intent owns becomes a candidate again: a
    /// close that may not have landed leaves open work behind.
    pub(super) fn commit_terminal_intent_close(
        &mut self,
        intent_id: IntentId,
    ) -> Result<CloseIntentOutcome, PersistenceError> {
        match self.resolver.store_mut().close_terminal_intent(intent_id) {
            Ok(outcome) => Ok(outcome),
            Err(error) => {
                let candidates = self.lane_candidates(intent_id);
                self.project_failed_lane_mutation(intent_id, candidates, &error);
                Err(error)
            }
        }
    }

    /// Shared tail for the single-lane doors: project the committed lane, or
    /// treat the door's own relay as the sole unknown-durability candidate.
    fn absorb_lane_result(
        &mut self,
        key: &LaneKey,
        result: Result<RecoveredLane, PersistenceError>,
    ) -> Result<RecoveredLane, PersistenceError> {
        match result {
            Ok(lane) => {
                self.project_committed_lane(&lane);
                Ok(lane)
            }
            Err(error) => {
                self.project_failed_lane_mutation(key.intent_id, [key.relay.clone()], &error);
                Err(error)
            }
        }
    }
}
