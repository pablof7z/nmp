//! Typed ownership for one exact local request-send attempt (#849/#774).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use nmp_grammar::{ConcreteFilter, DescriptorHash, RelaySessionKey};
use nmp_router::{SubId, WireDelta, WireOp};
use nmp_store::CoverageKey;
use nostr::Timestamp;

use super::{
    bootstrap_retry_delay_secs, Effect, EngineCore, EventFailureTarget, TransportRelayHandle,
};

/// Reducer-minted identity of one exact local request-send attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestAttemptId(pub(crate) u64);

/// A local transport refusal is a fact about this process, never the relay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalSendRefusal {
    SessionUnavailable,
    WorkerAdmissionRefused { handle: TransportRelayHandle },
}

/// Closed transport result for exactly one reducer-owned send attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestHandoffOutcome {
    Accepted {
        attempt_id: RequestAttemptId,
        handle: TransportRelayHandle,
    },
    Refused {
        attempt_id: RequestAttemptId,
        cause: LocalSendRefusal,
    },
}

impl RequestHandoffOutcome {
    pub(crate) fn attempt_id(&self) -> RequestAttemptId {
        match self {
            Self::Accepted { attempt_id, .. } | Self::Refused { attempt_id, .. } => *attempt_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RequestAttemptPurpose {
    Ordinary,
    Nip77LiveCandidate { plan_sub_id: SubId },
    Nip77Open { plan_sub_id: SubId },
    Nip77MissingIds { plan_sub_id: SubId },
    Nip77Backlog { plan_sub_id: SubId },
    Nip77Probe,
    Nip77Continue,
}

#[derive(Debug, Clone)]
pub(super) struct RequestAttemptState {
    pub(super) session: RelaySessionKey,
    pub(super) sub_id: SubId,
    pub(super) filter_hash: DescriptorHash,
    pub(super) filter: ConcreteFilter,
    pub(super) coverage_claims: std::collections::BTreeSet<CoverageKey>,
    pub(super) owner_demands: std::collections::BTreeSet<nmp_router::DemandKey>,
    pub(super) replay: bool,
    pub(super) event_failure_target: EventFailureTarget,
    pub(super) request_revision: Option<u64>,
    /// Refusals already observed for this one semantic retry goal.
    /// Carried through Attempting so backoff never resets when the retry
    /// record leaves the deadline map for dispatch.
    pub(super) retry_failures: u32,
    pub(super) purpose: RequestAttemptPurpose,
}

pub(super) struct RequestSend<'a> {
    pub(super) session: &'a RelaySessionKey,
    pub(super) sub_id: &'a SubId,
    pub(super) filter: &'a ConcreteFilter,
    pub(super) coverage_claims: BTreeSet<CoverageKey>,
    pub(super) owner_demands: BTreeSet<nmp_router::DemandKey>,
    pub(super) replay: bool,
    pub(super) event_failure_target: EventFailureTarget,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum RequestRetryKey {
    Ordinary(RelaySessionKey, SubId),
    Nip77LiveCandidate(SubId),
    Nip77MissingIds(SubId),
    Nip77Backlog(SubId),
}

#[derive(Debug, Clone)]
pub(super) struct PendingRequestRetry {
    pub(super) attempt: RequestAttemptState,
    pub(super) due: Timestamp,
    pub(super) failures: u32,
}

impl RequestAttemptState {
    pub(super) fn retry_key(&self) -> Option<RequestRetryKey> {
        match &self.purpose {
            RequestAttemptPurpose::Ordinary => Some(RequestRetryKey::Ordinary(
                self.session.clone(),
                self.sub_id.clone(),
            )),
            RequestAttemptPurpose::Nip77LiveCandidate { plan_sub_id } => {
                Some(RequestRetryKey::Nip77LiveCandidate(plan_sub_id.clone()))
            }
            RequestAttemptPurpose::Nip77MissingIds { plan_sub_id } => {
                Some(RequestRetryKey::Nip77MissingIds(plan_sub_id.clone()))
            }
            RequestAttemptPurpose::Nip77Backlog { plan_sub_id } => {
                Some(RequestRetryKey::Nip77Backlog(plan_sub_id.clone()))
            }
            RequestAttemptPurpose::Nip77Open { .. }
            | RequestAttemptPurpose::Nip77Probe
            | RequestAttemptPurpose::Nip77Continue => None,
        }
    }
}

impl RequestAttemptPurpose {
    pub(super) fn plan_sub_id(&self) -> Option<&SubId> {
        match self {
            Self::Nip77LiveCandidate { plan_sub_id }
            | Self::Nip77Open { plan_sub_id }
            | Self::Nip77MissingIds { plan_sub_id }
            | Self::Nip77Backlog { plan_sub_id } => Some(plan_sub_id),
            Self::Ordinary | Self::Nip77Probe | Self::Nip77Continue => None,
        }
    }

    pub(super) fn evidence_sub_id(&self, physical_sub_id: &SubId) -> SubId {
        self.plan_sub_id()
            .cloned()
            .unwrap_or_else(|| physical_sub_id.clone())
    }
}

/// Every local request-send attempt this reducer owns, plus the retries
/// parked behind them.
///
/// Fields are PRIVATE and this is a sibling module of `write`, `query`,
/// `observation` and `auth_transport`, so none of them can reach the maps —
/// only this file can, and the reverse-index invariants below are therefore
/// enforceable rather than merely documented (#1606).
///
/// It holds state and the invariants over that state, and nothing else: no
/// `store`, no `router`, no `resolver`, no `Effect`. Anything that has to
/// emit is orchestration and stays on `EngineCore`. This is the
/// `AttributionState` contract, verbatim.
#[derive(Debug, Default)]
pub(super) struct RequestAttempts {
    attempts: HashMap<RequestAttemptId, RequestAttemptState>,
    by_sub: HashMap<SubId, BTreeSet<RequestAttemptId>>,
    by_session: HashMap<RelaySessionKey, BTreeSet<RequestAttemptId>>,
    next_id: Option<u64>,
    retries: BTreeMap<RequestRetryKey, PendingRequestRetry>,
    retry_by_sub: HashMap<SubId, RequestRetryKey>,
    retries_by_session: HashMap<RelaySessionKey, BTreeSet<RequestRetryKey>>,
}

/// The census contribution, so `bench_ownership_census` counts this owner's
/// state without naming its maps. Deliberately `pub(super)` and NOT nested
/// into `CoreOwnershipCensus`, which stays a flat `pub` struct.
#[cfg(any(test, feature = "bench-instrumentation"))]
pub(super) struct RequestAttemptCounts {
    pub(super) attempts: usize,
    pub(super) sub_keys: usize,
    pub(super) sub_edges: usize,
    pub(super) session_keys: usize,
    pub(super) session_edges: usize,
    pub(super) retry_jobs: usize,
    pub(super) retry_sub_keys: usize,
    pub(super) retry_session_keys: usize,
    pub(super) retry_session_edges: usize,
}

impl RequestAttempts {
    pub(super) fn new() -> Self {
        Self {
            next_id: Some(0),
            ..Self::default()
        }
    }

    pub(super) fn get(&self, attempt_id: RequestAttemptId) -> Option<&RequestAttemptState> {
        self.attempts.get(&attempt_id)
    }

    pub(super) fn mint(&mut self, attempt: RequestAttemptState) -> RequestAttemptId {
        let value = self
            .next_id
            .expect("request attempt identity space exhausted");
        self.next_id = value.checked_add(1);
        let id = RequestAttemptId(value);
        self.by_sub
            .entry(attempt.sub_id.clone())
            .or_default()
            .insert(id);
        self.by_session
            .entry(attempt.session.clone())
            .or_default()
            .insert(id);
        let previous = self.attempts.insert(id, attempt);
        debug_assert!(previous.is_none());
        id
    }

    pub(super) fn take(&mut self, outcome: &RequestHandoffOutcome) -> Option<RequestAttemptState> {
        self.remove(outcome.attempt_id())
    }

    pub(super) fn retire_for_sub(&mut self, sub_id: &SubId) {
        let attempts = self.by_sub.remove(sub_id).unwrap_or_default();
        for attempt_id in attempts {
            self.remove(attempt_id);
        }
        self.cancel_retry_for_sub(sub_id);
    }

    pub(super) fn retire_for_session(&mut self, session: &RelaySessionKey) {
        let attempts = self.by_session.remove(session).unwrap_or_default();
        for attempt_id in attempts {
            self.remove(attempt_id);
        }
        let retries = self.retries_by_session.remove(session).unwrap_or_default();
        for retry in retries {
            self.remove_retry(&retry);
        }
    }

    fn remove(&mut self, attempt_id: RequestAttemptId) -> Option<RequestAttemptState> {
        let attempt = self.attempts.remove(&attempt_id)?;
        if let Some(ids) = self.by_sub.get_mut(&attempt.sub_id) {
            ids.remove(&attempt_id);
            if ids.is_empty() {
                self.by_sub.remove(&attempt.sub_id);
            }
        }
        if let Some(ids) = self.by_session.get_mut(&attempt.session) {
            ids.remove(&attempt_id);
            if ids.is_empty() {
                self.by_session.remove(&attempt.session);
            }
        }
        Some(attempt)
    }

    /// `now` is an argument rather than a read of `EngineCore::clock`: the
    /// owner holds no clock, exactly as it holds no store.
    pub(super) fn schedule_retry(&mut self, attempt: RequestAttemptState, now: Timestamp) {
        let Some(key) = attempt.retry_key() else {
            return;
        };
        let failures = attempt.retry_failures.saturating_add(1);
        self.remove_retry(&key);
        self.retry_by_sub
            .insert(attempt.sub_id.clone(), key.clone());
        self.retries_by_session
            .entry(attempt.session.clone())
            .or_default()
            .insert(key.clone());
        self.retries.insert(
            key,
            PendingRequestRetry {
                attempt,
                due: now + bootstrap_retry_delay_secs(failures),
                failures,
            },
        );
    }

    pub(super) fn clear_retry_for_attempt(&mut self, attempt: &RequestAttemptState) {
        if let Some(key) = attempt.retry_key() {
            self.remove_retry(&key);
        }
    }

    pub(super) fn cancel_retry_for_sub(&mut self, sub_id: &SubId) {
        if let Some(key) = self.retry_by_sub.remove(sub_id) {
            self.remove_retry(&key);
        }
    }

    fn remove_retry(&mut self, key: &RequestRetryKey) -> Option<PendingRequestRetry> {
        let pending = self.retries.remove(key)?;
        self.retry_by_sub.remove(&pending.attempt.sub_id);
        if let Some(keys) = self.retries_by_session.get_mut(&pending.attempt.session) {
            keys.remove(key);
            if keys.is_empty() {
                self.retries_by_session.remove(&pending.attempt.session);
            }
        }
        Some(pending)
    }

    /// When the retry parked behind `sub_id` is due, for the deferred-request
    /// observation fact. One call instead of the two-map hop I5 governs.
    pub(super) fn retry_due_for_sub(&self, sub_id: &SubId) -> Option<Timestamp> {
        let key = self.retry_by_sub.get(sub_id)?;
        self.retries.get(key).map(|retry| retry.due)
    }

    /// Every request awaiting a terminal, as the `(session, evidence sub id)`
    /// pairs acquisition evidence is keyed by.
    pub(super) fn awaiting_evidence_keys(&self) -> BTreeSet<(RelaySessionKey, SubId)> {
        self.attempts
            .values()
            .chain(self.retries.values().map(|retry| &retry.attempt))
            .map(|attempt| {
                (
                    attempt.session.clone(),
                    attempt.purpose.evidence_sub_id(&attempt.sub_id),
                )
            })
            .collect()
    }

    /// The attempt behind every retry parked on one session.
    pub(super) fn retried_attempts_for_session<'a>(
        &'a self,
        session: &'a RelaySessionKey,
    ) -> impl Iterator<Item = &'a RequestAttemptState> + 'a {
        self.retries
            .values()
            .map(|retry| &retry.attempt)
            .filter(move |attempt| &attempt.session == session)
    }

    /// The earliest parked retry deadline, for the reducer's wake schedule.
    pub(super) fn next_retry_due(&self) -> Option<Timestamp> {
        self.retries.values().map(|pending| pending.due).min()
    }

    /// Retry keys whose deadline has passed. Selection only -- dispatching
    /// them emits wire effects and therefore stays at the root.
    pub(super) fn due_retry_keys(&self, now: Timestamp) -> Vec<RequestRetryKey> {
        self.retries
            .iter()
            .filter(|(_, pending)| pending.due <= now)
            .map(|(key, _)| key.clone())
            .collect()
    }

    pub(super) fn take_retry(&mut self, key: &RequestRetryKey) -> Option<PendingRequestRetry> {
        self.remove_retry(key)
    }

    /// Carry the failure count of the retry that produced `attempt_id` onto
    /// the freshly minted attempt.
    pub(super) fn set_retry_failures(&mut self, attempt_id: RequestAttemptId, failures: u32) {
        self.attempts
            .get_mut(&attempt_id)
            .expect("the retry dispatch just minted its exact attempt")
            .retry_failures = failures;
    }

    /// Widen the coverage/owner metadata of every attempt and parked retry
    /// under `role_sub_ids`.
    ///
    /// The fan-out is an ARGUMENT, not a read: computing which role
    /// subscriptions belong to a plan needs NIP-77 state this owner has no
    /// business seeing, so the root computes it and hands the answer in.
    pub(super) fn extend_metadata(
        &mut self,
        role_sub_ids: &BTreeSet<SubId>,
        update: &nmp_router::RequestMetadataUpdate,
    ) {
        self.for_each_metadata_target(role_sub_ids, &mut |attempt| {
            attempt
                .coverage_claims
                .extend(update.added_coverage_claims.iter().copied());
            attempt
                .owner_demands
                .extend(update.added_owner_demands.iter().copied());
        });
    }

    pub(super) fn remove_metadata(
        &mut self,
        role_sub_ids: &BTreeSet<SubId>,
        removal: &nmp_router::RequestMetadataRemoval,
    ) {
        self.for_each_metadata_target(role_sub_ids, &mut |attempt| {
            attempt
                .coverage_claims
                .retain(|claim| !removal.removed_coverage_claims.contains(claim));
            attempt
                .owner_demands
                .retain(|demand| !removal.removed_owner_demands.contains(demand));
        });
    }

    /// I5, in one place: both reverse indexes are exact, and both `expect`s
    /// that say so are internal to the owner that maintains them.
    fn for_each_metadata_target(
        &mut self,
        role_sub_ids: &BTreeSet<SubId>,
        apply: &mut dyn FnMut(&mut RequestAttemptState),
    ) {
        for role_sub_id in role_sub_ids {
            let attempt_ids = self.by_sub.get(role_sub_id).cloned().unwrap_or_default();
            for attempt_id in attempt_ids {
                apply(
                    self.attempts
                        .get_mut(&attempt_id)
                        .expect("request attempt reverse index is exact"),
                );
            }
            if let Some(retry_key) = self.retry_by_sub.get(role_sub_id).cloned() {
                apply(
                    &mut self
                        .retries
                        .get_mut(&retry_key)
                        .expect("request retry reverse index is exact")
                        .attempt,
                );
            }
        }
    }

    #[cfg(any(test, feature = "bench-instrumentation"))]
    pub(super) fn counts(&self) -> RequestAttemptCounts {
        RequestAttemptCounts {
            attempts: self.attempts.len(),
            sub_keys: self.by_sub.len(),
            sub_edges: self.by_sub.values().map(BTreeSet::len).sum(),
            session_keys: self.by_session.len(),
            session_edges: self.by_session.values().map(BTreeSet::len).sum(),
            retry_jobs: self.retries.len(),
            retry_sub_keys: self.retry_by_sub.len(),
            retry_session_keys: self.retries_by_session.len(),
            retry_session_edges: self.retries_by_session.values().map(BTreeSet::len).sum(),
        }
    }
}

impl EngineCore {
    /// Which role subscriptions one plan's metadata update applies to.
    ///
    /// This is the NIP-77 fan-out the attempt owner deliberately cannot see:
    /// a plan's live candidate, its reconciliation session, and its backlog
    /// children all carry the plan's claims. Computed here, where that state
    /// lives, and handed to the owner as a set. Cluster 11's owner will
    /// provide this directly once it exists.
    fn role_sub_ids_for_plan(&self, plan_sub_id: &SubId) -> BTreeSet<SubId> {
        let mut role_sub_ids = BTreeSet::from([plan_sub_id.clone()]);
        role_sub_ids.extend(
            self.pending_neg_handoffs_by_plan
                .get(plan_sub_id)
                .into_iter()
                .flatten()
                .cloned(),
        );
        role_sub_ids.extend(
            self.neg_sessions_by_plan
                .get(plan_sub_id)
                .into_iter()
                .flatten()
                .cloned(),
        );
        for child in self
            .pending_backfills_by_plan
            .get(plan_sub_id)
            .cloned()
            .unwrap_or_default()
        {
            match self.pending_backfills.get(&child) {
                // The ids-only fetch is not coverage proof and deliberately
                // owns no plan claims. The retained NEG snapshot is extended
                // separately by `extend_plan_execution_metadata`.
                Some(super::TemporaryReq::MissingIds { .. }) => {}
                Some(super::TemporaryReq::Backlog { .. }) => {
                    role_sub_ids.insert(child);
                }
                Some(super::TemporaryReq::BacklogActivatesLive { live_sub_id, .. }) => {
                    role_sub_ids.insert(child);
                    role_sub_ids.insert(live_sub_id.clone());
                }
                None => {}
            }
        }
        role_sub_ids
    }

    pub(super) fn extend_request_attempt_metadata(
        &mut self,
        update: &nmp_router::RequestMetadataUpdate,
    ) {
        let role_sub_ids = self.role_sub_ids_for_plan(&update.sub_id);
        self.attempts.extend_metadata(&role_sub_ids, update);
    }

    pub(super) fn remove_request_attempt_metadata(
        &mut self,
        removal: &nmp_router::RequestMetadataRemoval,
    ) {
        let role_sub_ids = self.role_sub_ids_for_plan(&removal.sub_id);
        self.attempts.remove_metadata(&role_sub_ids, removal);
    }

    /// Dispatch every retry whose deadline has passed.
    ///
    /// Selection and bookkeeping are the owner's; re-sending is not — it
    /// mints a fresh attempt through attribution and emits a wire effect, so
    /// the body stays here.
    pub(super) fn retry_due_request_attempts(&mut self, now: Timestamp, effects: &mut Vec<Effect>) {
        for key in self.attempts.due_retry_keys(now) {
            let Some(pending) = self.attempts.take_retry(&key) else {
                continue;
            };
            if !self.request_retry_is_current(&pending.attempt) {
                continue;
            }
            let attempt = pending.attempt;
            let session = attempt.session.clone();
            let sub_id = attempt.sub_id.clone();
            let filter = attempt.filter.clone();
            let (_, attempt_id) = self.record_observed_request_with_purpose(
                RequestSend {
                    session: &session,
                    sub_id: &sub_id,
                    filter: &filter,
                    coverage_claims: attempt.coverage_claims,
                    owner_demands: attempt.owner_demands,
                    replay: attempt.replay,
                    event_failure_target: attempt.event_failure_target,
                },
                attempt.purpose,
            );
            self.attempts
                .set_retry_failures(attempt_id, pending.failures);
            effects.push(Effect::Wire(self.attempted_wire_delta(WireDelta {
                ops: vec![(session, vec![WireOp::Req(sub_id, filter)])],
            })));
        }
    }

    fn request_retry_is_current(&self, attempt: &RequestAttemptState) -> bool {
        match &attempt.purpose {
            RequestAttemptPurpose::Ordinary => self
                .plan_execution_metadata
                .get(&attempt.sub_id)
                .is_some_and(|metadata| metadata.filter == attempt.filter),
            RequestAttemptPurpose::Nip77LiveCandidate { plan_sub_id } => self
                .pending_neg_handoffs_by_plan
                .get(plan_sub_id)
                .is_some_and(|children| children.contains(&attempt.sub_id)),
            RequestAttemptPurpose::Nip77MissingIds { plan_sub_id }
            | RequestAttemptPurpose::Nip77Backlog { plan_sub_id } => self
                .pending_backfills_by_plan
                .get(plan_sub_id)
                .is_some_and(|children| children.contains(&attempt.sub_id)),
            RequestAttemptPurpose::Nip77Open { .. }
            | RequestAttemptPurpose::Nip77Probe
            | RequestAttemptPurpose::Nip77Continue => false,
        }
    }
}
