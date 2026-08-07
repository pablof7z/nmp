//! A fault-injecting `EventStore` for the durable lane doors.
//!
//! Deliberately NOT under `src/core`: this is a STORE implementation — the
//! far side of the projection door — not reducer code, and
//! `every_core_lane_mutation_uses_the_projection_door` correctly refuses any
//! raw lane-door call it finds in a core module.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use nmp_grammar::ContextualAtom;
use nmp_store::{
    AcceptOutcome, AcceptWrite, AuthDenial, CloseIntentOutcome, CompensateOutcome,
    CompensationReason, CoverageInterval, CoverageKey, EventStore, GcReport, GcRetentionSet,
    InsertOutcome, IntentId, PersistenceError, PersistenceFault, PromoteOutcome,
    PublishQueueAttempt, PublishQueueAttemptDetails, PublishQueueAttemptHandoff,
    PublishQueueAttemptOutcome, PublishQueueDeadline, PublishQueueIntent, PublishQueueLane,
    PublishQueueLaneKey, PublishQueuePostHandoffState, PublishQueueReceipt,
    PublishQueueRouteRevision, PublishQueueTransientCause, RefuseReason, RelayObserved,
    RemoveQueueEntryOutcome, RetractReason, StoredEvent,
};
use nostr::{Event, Event as SignedEvent, EventId, PublicKey, RelayUrl, Timestamp};

// ---- fault injection ---------------------------------------------------

/// Which durable lane door is currently refusing, and how it classifies its
/// refusal. Both classifications matter: `Io` is `DurabilityOutcome::Unknown`
/// (the transition may have landed) while `Invariant` is `Absent` (the
/// post-commit decode path at `publish_queue_ops.rs`), and #1000's stuck-forever
/// shape is reachable through either.
#[derive(Default)]
pub(crate) struct LaneFaultState {
    bootstrap: Option<PersistenceFault>,
    route_revisions: bool,
    auth_denial: Option<PersistenceFault>,
    /// Refuse the NEXT `record_lane_handoff` only. One shot is the whole
    /// point: the transport's handoff result is itself one-shot (#93), so a
    /// refusal that heals immediately still consumes the lane's only exit.
    handoff_once: Option<PersistenceFault>,
    bootstrap_calls: u32,
}

#[derive(Clone, Default)]
pub(crate) struct LaneFaults(Arc<Mutex<LaneFaultState>>);

impl LaneFaults {
    pub(crate) fn fail_bootstrap(&self, fault: PersistenceFault) {
        self.0.lock().unwrap().bootstrap = Some(fault);
    }

    pub(crate) fn fail_route_revisions(&self) {
        self.0.lock().unwrap().route_revisions = true;
    }

    pub(crate) fn fail_auth_denial(&self, fault: PersistenceFault) {
        self.0.lock().unwrap().auth_denial = Some(fault);
    }

    pub(crate) fn fail_handoff_once(&self, fault: PersistenceFault) {
        self.0.lock().unwrap().handoff_once = Some(fault);
    }

    pub(crate) fn heal(&self) {
        let mut state = self.0.lock().unwrap();
        state.bootstrap = None;
        state.route_revisions = false;
        state.auth_denial = None;
        state.handoff_once = None;
    }

    pub(crate) fn bootstrap_calls(&self) -> u32 {
        self.0.lock().unwrap().bootstrap_calls
    }

    fn take_bootstrap_failure(&self) -> Option<PersistenceError> {
        let mut state = self.0.lock().unwrap();
        state.bootstrap_calls = state.bootstrap_calls.saturating_add(1);
        state.bootstrap.map(|fault| {
            PersistenceError::new(fault, "injected lane bootstrap failure".to_string())
        })
    }

    fn take_route_revision_failure(&self) -> Option<PersistenceError> {
        self.0.lock().unwrap().route_revisions.then(|| {
            PersistenceError::new(
                PersistenceFault::Io,
                "injected route revision read failure".to_string(),
            )
        })
    }

    fn take_handoff_failure(&self) -> Option<PersistenceError> {
        self.0
            .lock()
            .unwrap()
            .handoff_once
            .take()
            .map(|fault| PersistenceError::new(fault, "injected lane handoff failure".to_string()))
    }

    fn take_auth_denial_failure(&self) -> Option<PersistenceError> {
        self.0
            .lock()
            .unwrap()
            .auth_denial
            .map(|fault| PersistenceError::new(fault, "injected AUTH denial failure".to_string()))
    }
}

/// A delegating store whose lane-bootstrap and route-revision reads can be
/// made to fail and then healed. Generic over the backend so the same double
/// covers the in-memory live path and the redb restart path.
pub(crate) struct FaultyLaneStore<S> {
    inner: S,
    faults: LaneFaults,
}

impl<S: EventStore> FaultyLaneStore<S> {
    pub(crate) fn new(inner: S, faults: LaneFaults) -> Self {
        Self { inner, faults }
    }
}

impl<S: EventStore> EventStore for FaultyLaneStore<S> {
    fn bootstrap_publish_queue_lanes(
        &mut self,
        intent_id: IntentId,
    ) -> Result<Vec<PublishQueueLane>, PersistenceError> {
        if let Some(error) = self.faults.take_bootstrap_failure() {
            return Err(error);
        }
        self.inner.bootstrap_publish_queue_lanes(intent_id)
    }
    fn recover_route_revisions(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<PublishQueueRouteRevision>, PersistenceError> {
        if let Some(error) = self.faults.take_route_revision_failure() {
            return Err(error);
        }
        self.inner.recover_route_revisions(intent_id)
    }

    fn recover_publish_queue_lanes(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<PublishQueueLane>, PersistenceError> {
        self.inner.recover_publish_queue_lanes(intent_id)
    }
    fn due_publish_queue_deadlines(
        &self,
        now: Timestamp,
        limit: usize,
    ) -> Result<Vec<PublishQueueDeadline>, PersistenceError> {
        self.inner.due_publish_queue_deadlines(now, limit)
    }
    fn next_publish_queue_deadline(&self) -> Result<Option<Timestamp>, PersistenceError> {
        self.inner.next_publish_queue_deadline()
    }
    fn set_lane_waiting(
        &mut self,
        key: &PublishQueueLaneKey,
        revision: u64,
        auth: bool,
    ) -> Result<PublishQueueLane, PersistenceError> {
        self.inner.set_lane_waiting(key, revision, auth)
    }
    fn set_lane_eligible(
        &mut self,
        key: &PublishQueueLaneKey,
        revision: u64,
        since: Timestamp,
    ) -> Result<PublishQueueLane, PersistenceError> {
        self.inner.set_lane_eligible(key, revision, since)
    }
    fn set_lane_transient(
        &mut self,
        key: &PublishQueueLaneKey,
        revision: u64,
        ordinal: u64,
        eligible_at: Timestamp,
        cause: PublishQueueTransientCause,
        raw_reason: Option<String>,
    ) -> Result<PublishQueueLane, PersistenceError> {
        self.inner
            .set_lane_transient(key, revision, ordinal, eligible_at, cause, raw_reason)
    }
    #[allow(clippy::too_many_arguments)]
    fn suspend_lane_attempt(
        &mut self,
        key: &PublishQueueLaneKey,
        revision: u64,
        ordinal: u64,
        at: Timestamp,
        cause: PublishQueueTransientCause,
        raw_reason: Option<String>,
        auth: bool,
    ) -> Result<PublishQueueLane, PersistenceError> {
        self.inner
            .suspend_lane_attempt(key, revision, ordinal, at, cause, raw_reason, auth)
    }
    fn start_lane_attempt(
        &mut self,
        key: &PublishQueueLaneKey,
        revision: u64,
        event: SignedEvent,
        started_at: Timestamp,
    ) -> Result<(PublishQueueAttempt, PublishQueueLane), PersistenceError> {
        self.inner
            .start_lane_attempt(key, revision, event, started_at)
    }
    fn record_lane_handoff(
        &mut self,
        key: &PublishQueueLaneKey,
        revision: u64,
        ordinal: u64,
        detail: PublishQueueAttemptHandoff,
        next: PublishQueuePostHandoffState,
    ) -> Result<PublishQueueLane, PersistenceError> {
        if let Some(error) = self.faults.take_handoff_failure() {
            return Err(error);
        }
        self.inner
            .record_lane_handoff(key, revision, ordinal, detail, next)
    }
    fn finish_lane_attempt(
        &mut self,
        key: &PublishQueueLaneKey,
        revision: u64,
        ordinal: u64,
        outcome: PublishQueueAttemptOutcome,
        finished_at: Timestamp,
    ) -> Result<PublishQueueLane, PersistenceError> {
        self.inner
            .finish_lane_attempt(key, revision, ordinal, outcome, finished_at)
    }
    fn deny_lane_auth(
        &mut self,
        key: &PublishQueueLaneKey,
        revision: u64,
        denial: AuthDenial,
    ) -> Result<PublishQueueLane, PersistenceError> {
        if let Some(error) = self.faults.take_auth_denial_failure() {
            return Err(error);
        }
        self.inner.deny_lane_auth(key, revision, denial)
    }
    fn recover_attempt_details(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<PublishQueueAttemptDetails>, PersistenceError> {
        self.inner.recover_attempt_details(intent_id)
    }
    fn close_terminal_intent(
        &mut self,
        intent_id: IntentId,
    ) -> Result<CloseIntentOutcome, PersistenceError> {
        self.inner.close_terminal_intent(intent_id)
    }

    fn compensate_write_with_state(
        &mut self,
        intent_id: IntentId,
        reason: CompensationReason,
    ) -> Result<CompensateOutcome, PersistenceError> {
        self.inner.compensate_write_with_state(intent_id, reason)
    }
    fn insert(
        &mut self,
        event: Event,
        from: RelayObserved,
    ) -> Result<InsertOutcome, PersistenceError> {
        self.inner.insert(event, from)
    }
    fn query(&self, filter: &nostr::Filter) -> Result<Vec<StoredEvent>, PersistenceError> {
        self.inner.query(filter)
    }
    fn remove(
        &mut self,
        id: EventId,
        reason: RetractReason,
    ) -> Result<Option<StoredEvent>, PersistenceError> {
        self.inner.remove(id, reason)
    }
    fn expire_due(&mut self, now: Timestamp) -> Result<Vec<StoredEvent>, PersistenceError> {
        self.inner.expire_due(now)
    }
    fn next_expiration(&self) -> Result<Option<Timestamp>, PersistenceError> {
        self.inner.next_expiration()
    }
    fn record_coverage(
        &mut self,
        claims: &[(ContextualAtom, RelayUrl, CoverageInterval)],
    ) -> Result<(), PersistenceError> {
        self.inner.record_coverage(claims)
    }
    fn get_coverage(
        &self,
        key: CoverageKey,
        relay: &RelayUrl,
    ) -> Result<Option<CoverageInterval>, PersistenceError> {
        self.inner.get_coverage(key, relay)
    }
    fn gc(&mut self, claims: &GcRetentionSet) -> Result<GcReport, PersistenceError> {
        self.inner.gc(claims)
    }
    fn accept_write(&mut self, accept: AcceptWrite) -> Result<AcceptOutcome, PersistenceError> {
        self.inner.accept_write(accept)
    }
    fn promote_signed(
        &mut self,
        intent_id: IntentId,
        verified: nmp_store::VerifiedSignature,
    ) -> Result<PromoteOutcome, PersistenceError> {
        self.inner.promote_signed(intent_id, verified)
    }
    fn compensate_write(
        &mut self,
        intent_id: IntentId,
    ) -> Result<CompensateOutcome, PersistenceError> {
        self.inner.compensate_write(intent_id)
    }
    fn recover_publish_queue(&self) -> Result<Vec<PublishQueueIntent>, PersistenceError> {
        self.inner.recover_publish_queue()
    }
    fn reattach_receipt(
        &self,
        receipt_id: u64,
    ) -> Result<Option<PublishQueueReceipt>, PersistenceError> {
        self.inner.reattach_receipt(receipt_id)
    }
    fn lookup_correlation(&self, token: &str) -> Result<Option<u64>, PersistenceError> {
        self.inner.lookup_correlation(token)
    }
    fn record_route_revision(
        &mut self,
        intent_id: IntentId,
        relays: BTreeSet<RelayUrl>,
    ) -> Result<PublishQueueRouteRevision, PersistenceError> {
        self.inner.record_route_revision(intent_id, relays)
    }
    fn recover_attempts(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<PublishQueueAttempt>, PersistenceError> {
        self.inner.recover_attempts(intent_id)
    }
    fn enumerate_publish_queue_receipts(
        &self,
    ) -> Result<Vec<PublishQueueReceipt>, PersistenceError> {
        self.inner.enumerate_publish_queue_receipts()
    }
    fn remove_publish_queue_entry(
        &mut self,
        receipt_id: u64,
    ) -> Result<RemoveQueueEntryOutcome, PersistenceError> {
        self.inner.remove_publish_queue_entry(receipt_id)
    }
    fn accept_refused(
        &mut self,
        frozen_id: EventId,
        expected_pubkey: PublicKey,
        reason: RefuseReason,
    ) -> Result<u64, PersistenceError> {
        self.inner
            .accept_refused(frozen_id, expected_pubkey, reason)
    }
}
