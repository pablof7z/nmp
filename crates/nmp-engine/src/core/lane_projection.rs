//! The one reducer-owned door for durable lane projection.
//!
//! Store operations remain authoritative. This module consumes their exact
//! committed post-state and keeps the rebuildable in-memory worker projection
//! synchronized before ordinary effect dispatch can ask which sessions remain
//! owned.

use super::*;

impl CoreState {
    /// Reset one pending write's lane projection to empty ahead of a
    /// replaceable-operation successor rewrite.
    ///
    /// A member rewritten onto a new generation owns no lane the old
    /// generation minted: nothing may re-attach to them, so the projection
    /// resets to empty exactly as an exact rebuild against an empty recovered
    /// lane set would reset it.
    pub(in crate::core) fn reset_lane_projection_for_successor(&mut self, id: ReceiptId) {
        self.pending.reset_lane_projection(id);
    }

    /// Apply one successful store mutation's exact post-state.
    fn apply_committed_lane(&mut self, lane: &PublishQueueLane) {
        if let Some(id) = self.pending.receipt_for_intent(lane.key.intent_id) {
            self.pending.apply_committed_lane(id, lane);
        }
    }

    fn commit_lane_transition<T>(
        &mut self,
        operation: impl FnOnce(&mut RedbStore) -> Result<(T, PublishQueueLane), PersistenceError>,
    ) -> Result<(T, PublishQueueLane), PersistenceError> {
        let (value, lane) = operation(&mut self.store)?;
        self.apply_committed_lane(&lane);
        Ok((value, lane))
    }

    /// Establish (or re-establish) one intent's projection from the durable
    /// lane set, creating the lanes its recorded route revisions imply.
    ///
    /// A failure leaves the in-memory projection as it was and returns `Err`.
    /// The durable lanes are untouched, so the next boot rebuilds them from
    /// the store: what a failure here costs is progress, never the write.
    pub(in crate::core) fn bootstrap_projected_lanes(
        &mut self,
        intent_id: IntentId,
    ) -> Result<Vec<PublishQueueLane>, PersistenceError> {
        let lanes = self.store.bootstrap_publish_queue_lanes(intent_id)?;
        if let Some(id) = self.pending.receipt_for_intent(intent_id) {
            self.pending.replace_lane_projection(id, &lanes);
        }
        Ok(lanes)
    }

    /// Rebuild one semantic owner's volatile projection from the exact lanes
    /// installed by the atomic current-generation transition.
    ///
    /// Unlike ordinary lane bootstrap, this must not reconcile the current
    /// E2 lane state against retained E1 attempt history. The predecessor
    /// attempts are valid historical evidence, while the current event id is
    /// the fence that decides which physical lanes may run now.
    pub(in crate::core) fn recover_semantic_generation_lanes(
        &mut self,
        intent_id: IntentId,
        event_id: EventId,
    ) -> Result<Vec<PublishQueueLane>, PersistenceError> {
        let lanes = self.store.recover_publish_queue_lanes(intent_id)?;
        if lanes.iter().any(|lane| lane.key.event_id != event_id) {
            return Err(PersistenceError::new(
                "semantic lane recovery found a non-current event generation",
            ));
        }
        if let Some(id) = self.pending.receipt_for_intent(intent_id) {
            self.pending.replace_lane_projection(id, &lanes);
        }
        Ok(lanes)
    }

    pub(in crate::core) fn commit_lane_waiting(
        &mut self,
        key: &PublishQueueLaneKey,
        revision: u64,
        auth: bool,
    ) -> Result<PublishQueueLane, PersistenceError> {
        self.commit_lane_transition(|store| {
            store
                .set_lane_waiting(key, revision, auth)
                .map(|lane| ((), lane))
        })
        .map(|(_, lane)| lane)
    }

    pub(in crate::core) fn commit_lane_eligible(
        &mut self,
        key: &PublishQueueLaneKey,
        revision: u64,
        since: Timestamp,
    ) -> Result<PublishQueueLane, PersistenceError> {
        self.commit_lane_transition(|store| {
            store
                .set_lane_eligible(key, revision, since)
                .map(|lane| ((), lane))
        })
        .map(|(_, lane)| lane)
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::core) fn commit_lane_transient(
        &mut self,
        key: &PublishQueueLaneKey,
        revision: u64,
        ordinal: u64,
        eligible_at: Timestamp,
        cause: PublishQueueTransientCause,
        raw_reason: Option<String>,
    ) -> Result<PublishQueueLane, PersistenceError> {
        self.commit_lane_transition(|store| {
            store
                .set_lane_transient(key, revision, ordinal, eligible_at, cause, raw_reason)
                .map(|lane| ((), lane))
        })
        .map(|(_, lane)| lane)
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::core) fn commit_lane_suspension(
        &mut self,
        key: &PublishQueueLaneKey,
        revision: u64,
        ordinal: u64,
        at: Timestamp,
        cause: PublishQueueTransientCause,
        raw_reason: Option<String>,
        auth: bool,
    ) -> Result<PublishQueueLane, PersistenceError> {
        self.commit_lane_transition(|store| {
            store
                .suspend_lane_attempt(key, revision, ordinal, at, cause, raw_reason, auth)
                .map(|lane| ((), lane))
        })
        .map(|(_, lane)| lane)
    }

    pub(in crate::core) fn commit_lane_attempt_start(
        &mut self,
        key: &PublishQueueLaneKey,
        revision: u64,
        event: SignedEvent,
        started_at: Timestamp,
    ) -> Result<(nmp_store::PublishQueueAttempt, PublishQueueLane), PersistenceError> {
        self.commit_lane_transition(|store| {
            store.start_lane_attempt(key, revision, event, started_at)
        })
    }

    pub(in crate::core) fn commit_lane_handoff(
        &mut self,
        key: &PublishQueueLaneKey,
        revision: u64,
        ordinal: u64,
        detail: PublishQueueAttemptHandoff,
        next: PublishQueuePostHandoffState,
    ) -> Result<PublishQueueLane, PersistenceError> {
        self.commit_lane_transition(|store| {
            store
                .record_lane_handoff(key, revision, ordinal, detail, next)
                .map(|lane| ((), lane))
        })
        .map(|(_, lane)| lane)
    }

    pub(in crate::core) fn commit_lane_attempt_finish(
        &mut self,
        key: &PublishQueueLaneKey,
        revision: u64,
        ordinal: u64,
        outcome: PublishQueueAttemptOutcome,
        finished_at: Timestamp,
    ) -> Result<PublishQueueLane, PersistenceError> {
        self.commit_lane_transition(|store| {
            store
                .finish_lane_attempt(key, revision, ordinal, outcome, finished_at)
                .map(|lane| ((), lane))
        })
        .map(|(_, lane)| lane)
    }

    pub(in crate::core) fn commit_lane_auth_denied(
        &mut self,
        key: &PublishQueueLaneKey,
        revision: u64,
        denial: StoredAuthDenial,
    ) -> Result<PublishQueueLane, PersistenceError> {
        self.commit_lane_transition(|store| {
            store
                .deny_lane_auth(key, revision, denial)
                .map(|lane| ((), lane))
        })
        .map(|(_, lane)| lane)
    }

    /// Append a durable route revision through the projection door.
    ///
    /// A revision mints no lane by itself today: its paired
    /// [`Self::bootstrap_projected_lanes`] is what returns the committed
    /// `PublishQueueLane` set, so this applies no projection delta and the
    /// caller's own route-blocked bookkeeping already retains worker demand
    /// when the append fails.
    ///
    /// It is nonetheless a door rather than a direct `self.store` call
    /// because under #975 `Auto` re-executes its strategy at every send
    /// opportunity and appends a revision whenever resolution learns
    /// something new — at which point lane minting moves onto this path. The
    /// door plus the enumeration falsifier is what makes that future change
    /// fail mechanically instead of silently projecting nothing.
    pub(in crate::core) fn commit_route_revision(
        &mut self,
        intent_id: IntentId,
        relays: BTreeSet<RelayUrl>,
    ) -> Result<nmp_store::PublishQueueRouteRevision, PersistenceError> {
        self.store.record_route_revision(intent_id, relays)
    }

    /// Close one intent's open work through the projection door.
    ///
    /// The store door validates the all-terminal invariant transactionally,
    /// so the projection contributes no precondition of its own. A failure
    /// changes nothing: the caller keeps the pending write, and with it every
    /// relay the projection still owns, rather than retiring a worker on an
    /// unproven close.
    pub(in crate::core) fn commit_terminal_close(
        &mut self,
        intent_id: IntentId,
    ) -> Result<CloseIntentOutcome, PersistenceError> {
        self.store.close_terminal_intent(intent_id)
    }
}

