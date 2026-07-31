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
    AcceptOutcome, AcceptWrite, AuthDenial, CancelEphemeralOutcome, CloseIntentOutcome,
    CompensateOutcome, CompensationReason, CoverageInterval, CoverageKey, DeliveryAttempt,
    DeliveryAttemptDetails, DeliveryAttemptHandoff, DeliveryAttemptOutcome, DeliveryDeadline,
    DeliveryIntent, DeliveryLane, DeliveryLaneKey, DeliveryPostHandoffState, DeliveryReceipt,
    DeliveryRouteRevision, DeliveryTransientCause, EventStore, GcReport, GcRetentionSet,
    InsertOutcome, IntentId, PersistenceError, PersistenceFault, PromoteOutcome, RelayObserved,
    RetractReason, StoredEvent,
};
use nostr::{Event, Event as SignedEvent, EventId, PublicKey, RelayUrl, Timestamp};

// ---- fault injection ---------------------------------------------------

/// Which durable lane door is currently refusing, and how it classifies its
/// refusal. Both classifications matter: `Io` is `DurabilityOutcome::Unknown`
/// (the transition may have landed) while `Invariant` is `Absent` (the
/// post-commit decode path at `delivery_ops.rs`), and #1000's stuck-forever
/// shape is reachable through either.
#[derive(Default)]
pub(crate) struct LaneFaultState {
    bootstrap: Option<PersistenceFault>,
    route_revisions: bool,
    auth_denial: Option<PersistenceFault>,
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

    pub(crate) fn heal(&self) {
        let mut state = self.0.lock().unwrap();
        state.bootstrap = None;
        state.route_revisions = false;
        state.auth_denial = None;
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
    fn bootstrap_delivery_lanes(
        &mut self,
        intent_id: IntentId,
    ) -> Result<Vec<DeliveryLane>, PersistenceError> {
        if let Some(error) = self.faults.take_bootstrap_failure() {
            return Err(error);
        }
        self.inner.bootstrap_delivery_lanes(intent_id)
    }
    fn recover_route_revisions(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<DeliveryRouteRevision>, PersistenceError> {
        if let Some(error) = self.faults.take_route_revision_failure() {
            return Err(error);
        }
        self.inner.recover_route_revisions(intent_id)
    }

    fn recover_delivery_lanes(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<DeliveryLane>, PersistenceError> {
        self.inner.recover_delivery_lanes(intent_id)
    }
    fn due_delivery_deadlines(
        &self,
        now: Timestamp,
        limit: usize,
    ) -> Result<Vec<DeliveryDeadline>, PersistenceError> {
        self.inner.due_delivery_deadlines(now, limit)
    }
    fn next_delivery_deadline(&self) -> Result<Option<Timestamp>, PersistenceError> {
        self.inner.next_delivery_deadline()
    }
    fn set_lane_waiting(
        &mut self,
        key: &DeliveryLaneKey,
        revision: u64,
        auth: bool,
    ) -> Result<DeliveryLane, PersistenceError> {
        self.inner.set_lane_waiting(key, revision, auth)
    }
    fn set_lane_eligible(
        &mut self,
        key: &DeliveryLaneKey,
        revision: u64,
        since: Timestamp,
    ) -> Result<DeliveryLane, PersistenceError> {
        self.inner.set_lane_eligible(key, revision, since)
    }
    fn set_lane_transient(
        &mut self,
        key: &DeliveryLaneKey,
        revision: u64,
        ordinal: u64,
        eligible_at: Timestamp,
        cause: DeliveryTransientCause,
        raw_reason: Option<String>,
    ) -> Result<DeliveryLane, PersistenceError> {
        self.inner
            .set_lane_transient(key, revision, ordinal, eligible_at, cause, raw_reason)
    }
    #[allow(clippy::too_many_arguments)]
    fn suspend_lane_attempt(
        &mut self,
        key: &DeliveryLaneKey,
        revision: u64,
        ordinal: u64,
        at: Timestamp,
        cause: DeliveryTransientCause,
        raw_reason: Option<String>,
        auth: bool,
    ) -> Result<DeliveryLane, PersistenceError> {
        self.inner
            .suspend_lane_attempt(key, revision, ordinal, at, cause, raw_reason, auth)
    }
    fn start_lane_attempt(
        &mut self,
        key: &DeliveryLaneKey,
        revision: u64,
        event: SignedEvent,
        started_at: Timestamp,
    ) -> Result<(DeliveryAttempt, DeliveryLane), PersistenceError> {
        self.inner
            .start_lane_attempt(key, revision, event, started_at)
    }
    fn record_lane_handoff(
        &mut self,
        key: &DeliveryLaneKey,
        revision: u64,
        ordinal: u64,
        detail: DeliveryAttemptHandoff,
        next: DeliveryPostHandoffState,
    ) -> Result<DeliveryLane, PersistenceError> {
        self.inner
            .record_lane_handoff(key, revision, ordinal, detail, next)
    }
    fn finish_lane_attempt(
        &mut self,
        key: &DeliveryLaneKey,
        revision: u64,
        ordinal: u64,
        outcome: DeliveryAttemptOutcome,
        finished_at: Timestamp,
    ) -> Result<DeliveryLane, PersistenceError> {
        self.inner
            .finish_lane_attempt(key, revision, ordinal, outcome, finished_at)
    }
    fn deny_lane_auth(
        &mut self,
        key: &DeliveryLaneKey,
        revision: u64,
        denial: AuthDenial,
    ) -> Result<DeliveryLane, PersistenceError> {
        if let Some(error) = self.faults.take_auth_denial_failure() {
            return Err(error);
        }
        self.inner.deny_lane_auth(key, revision, denial)
    }
    fn recover_attempt_details(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<DeliveryAttemptDetails>, PersistenceError> {
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
    fn cancel_ephemeral_receipt(
        &mut self,
        receipt_id: u64,
    ) -> Result<CancelEphemeralOutcome, PersistenceError> {
        self.inner.cancel_ephemeral_receipt(receipt_id)
    }
    fn mark_ephemeral_signed(&mut self, receipt_id: u64) -> Result<bool, PersistenceError> {
        self.inner.mark_ephemeral_signed(receipt_id)
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
    fn next_expiration(&self) -> Option<Timestamp> {
        self.inner.next_expiration()
    }
    fn record_coverage(
        &mut self,
        claims: &[(ContextualAtom, RelayUrl, CoverageInterval)],
    ) -> Result<(), PersistenceError> {
        self.inner.record_coverage(claims)
    }
    fn get_coverage(&self, key: CoverageKey, relay: &RelayUrl) -> Option<CoverageInterval> {
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
        sig: nostr::secp256k1::schnorr::Signature,
    ) -> Result<PromoteOutcome, PersistenceError> {
        self.inner.promote_signed(intent_id, sig)
    }
    fn compensate_write(
        &mut self,
        intent_id: IntentId,
    ) -> Result<CompensateOutcome, PersistenceError> {
        self.inner.compensate_write(intent_id)
    }
    fn recover_delivery(&self) -> Result<Vec<DeliveryIntent>, PersistenceError> {
        self.inner.recover_delivery()
    }
    fn reattach_receipt(
        &self,
        receipt_id: u64,
    ) -> Result<Option<DeliveryReceipt>, PersistenceError> {
        self.inner.reattach_receipt(receipt_id)
    }
    fn lookup_correlation(&self, token: &str) -> Result<Option<u64>, PersistenceError> {
        self.inner.lookup_correlation(token)
    }
    fn record_route_revision(
        &mut self,
        intent_id: IntentId,
        relays: BTreeSet<RelayUrl>,
    ) -> Result<DeliveryRouteRevision, PersistenceError> {
        self.inner.record_route_revision(intent_id, relays)
    }
    fn recover_attempts(
        &self,
        intent_id: IntentId,
    ) -> Result<Vec<DeliveryAttempt>, PersistenceError> {
        self.inner.recover_attempts(intent_id)
    }
    fn accept_ephemeral(
        &mut self,
        frozen_id: EventId,
        expected_pubkey: PublicKey,
    ) -> Result<u64, PersistenceError> {
        self.inner.accept_ephemeral(frozen_id, expected_pubkey)
    }
}
