//! Typed ownership for one exact local request-send attempt (#849/#774).

use std::collections::BTreeSet;

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

impl EngineCore {
    pub(super) fn extend_request_attempt_metadata(
        &mut self,
        update: &nmp_router::RequestMetadataUpdate,
    ) {
        let mut role_sub_ids = BTreeSet::from([update.sub_id.clone()]);
        role_sub_ids.extend(
            self.pending_neg_handoffs_by_plan
                .get(&update.sub_id)
                .into_iter()
                .flatten()
                .cloned(),
        );
        role_sub_ids.extend(
            self.neg_sessions_by_plan
                .get(&update.sub_id)
                .into_iter()
                .flatten()
                .cloned(),
        );
        let backfills = self
            .pending_backfills_by_plan
            .get(&update.sub_id)
            .cloned()
            .unwrap_or_default();
        for child in backfills {
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
        for role_sub_id in role_sub_ids {
            let attempt_ids = self
                .request_attempts_by_sub
                .get(&role_sub_id)
                .cloned()
                .unwrap_or_default();
            for attempt_id in attempt_ids {
                let attempt = self
                    .request_attempts
                    .get_mut(&attempt_id)
                    .expect("request attempt reverse index is exact");
                attempt
                    .coverage_claims
                    .extend(update.added_coverage_claims.iter().copied());
                attempt
                    .owner_demands
                    .extend(update.added_owner_demands.iter().copied());
            }
            if let Some(retry_key) = self.request_retry_by_sub.get(&role_sub_id).cloned() {
                let retry = self
                    .pending_request_retries
                    .get_mut(&retry_key)
                    .expect("request retry reverse index is exact");
                retry
                    .attempt
                    .coverage_claims
                    .extend(update.added_coverage_claims.iter().copied());
                retry
                    .attempt
                    .owner_demands
                    .extend(update.added_owner_demands.iter().copied());
            }
        }
    }

    pub(super) fn remove_request_attempt_metadata(
        &mut self,
        removal: &nmp_router::RequestMetadataRemoval,
    ) {
        let mut role_sub_ids = BTreeSet::from([removal.sub_id.clone()]);
        role_sub_ids.extend(
            self.pending_neg_handoffs_by_plan
                .get(&removal.sub_id)
                .into_iter()
                .flatten()
                .cloned(),
        );
        role_sub_ids.extend(
            self.neg_sessions_by_plan
                .get(&removal.sub_id)
                .into_iter()
                .flatten()
                .cloned(),
        );
        for child in self
            .pending_backfills_by_plan
            .get(&removal.sub_id)
            .cloned()
            .unwrap_or_default()
        {
            match self.pending_backfills.get(&child) {
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
        for role_sub_id in role_sub_ids {
            let attempt_ids = self
                .request_attempts_by_sub
                .get(&role_sub_id)
                .cloned()
                .unwrap_or_default();
            for attempt_id in attempt_ids {
                let attempt = self
                    .request_attempts
                    .get_mut(&attempt_id)
                    .expect("request attempt reverse index is exact");
                attempt
                    .coverage_claims
                    .retain(|claim| !removal.removed_coverage_claims.contains(claim));
                attempt
                    .owner_demands
                    .retain(|demand| !removal.removed_owner_demands.contains(demand));
            }
            if let Some(retry_key) = self.request_retry_by_sub.get(&role_sub_id).cloned() {
                let retry = self
                    .pending_request_retries
                    .get_mut(&retry_key)
                    .expect("request retry reverse index is exact");
                retry
                    .attempt
                    .coverage_claims
                    .retain(|claim| !removal.removed_coverage_claims.contains(claim));
                retry
                    .attempt
                    .owner_demands
                    .retain(|demand| !removal.removed_owner_demands.contains(demand));
            }
        }
    }

    pub(super) fn mint_request_attempt(
        &mut self,
        attempt: RequestAttemptState,
    ) -> RequestAttemptId {
        let value = self
            .next_request_attempt
            .expect("request attempt identity space exhausted");
        self.next_request_attempt = value.checked_add(1);
        let id = RequestAttemptId(value);
        self.request_attempts_by_sub
            .entry(attempt.sub_id.clone())
            .or_default()
            .insert(id);
        self.request_attempts_by_session
            .entry(attempt.session.clone())
            .or_default()
            .insert(id);
        let previous = self.request_attempts.insert(id, attempt);
        debug_assert!(previous.is_none());
        id
    }

    pub(super) fn take_request_attempt(
        &mut self,
        outcome: &RequestHandoffOutcome,
    ) -> Option<RequestAttemptState> {
        self.remove_request_attempt(outcome.attempt_id())
    }

    pub(super) fn retire_request_attempts_for_sub(&mut self, sub_id: &SubId) {
        let attempts = self
            .request_attempts_by_sub
            .remove(sub_id)
            .unwrap_or_default();
        for attempt_id in attempts {
            self.remove_request_attempt(attempt_id);
        }
        self.cancel_request_retry_for_sub(sub_id);
    }

    pub(super) fn retire_request_attempts_for_session(&mut self, session: &RelaySessionKey) {
        let attempts = self
            .request_attempts_by_session
            .remove(session)
            .unwrap_or_default();
        for attempt_id in attempts {
            self.remove_request_attempt(attempt_id);
        }
        let retries = self
            .request_retries_by_session
            .remove(session)
            .unwrap_or_default();
        for retry in retries {
            self.remove_request_retry(&retry);
        }
    }

    fn remove_request_attempt(
        &mut self,
        attempt_id: RequestAttemptId,
    ) -> Option<RequestAttemptState> {
        let attempt = self.request_attempts.remove(&attempt_id)?;
        if let Some(ids) = self.request_attempts_by_sub.get_mut(&attempt.sub_id) {
            ids.remove(&attempt_id);
            if ids.is_empty() {
                self.request_attempts_by_sub.remove(&attempt.sub_id);
            }
        }
        if let Some(ids) = self.request_attempts_by_session.get_mut(&attempt.session) {
            ids.remove(&attempt_id);
            if ids.is_empty() {
                self.request_attempts_by_session.remove(&attempt.session);
            }
        }
        Some(attempt)
    }

    pub(super) fn schedule_request_retry(&mut self, attempt: RequestAttemptState) {
        let Some(key) = attempt.retry_key() else {
            return;
        };
        let failures = attempt.retry_failures.saturating_add(1);
        self.remove_request_retry(&key);
        self.request_retry_by_sub
            .insert(attempt.sub_id.clone(), key.clone());
        self.request_retries_by_session
            .entry(attempt.session.clone())
            .or_default()
            .insert(key.clone());
        self.pending_request_retries.insert(
            key,
            PendingRequestRetry {
                attempt,
                due: self.clock + bootstrap_retry_delay_secs(failures),
                failures,
            },
        );
    }

    pub(super) fn clear_request_retry_for_attempt(&mut self, attempt: &RequestAttemptState) {
        if let Some(key) = attempt.retry_key() {
            self.remove_request_retry(&key);
        }
    }

    pub(super) fn cancel_request_retry_for_sub(&mut self, sub_id: &SubId) {
        if let Some(key) = self.request_retry_by_sub.remove(sub_id) {
            self.remove_request_retry(&key);
        }
    }

    fn remove_request_retry(&mut self, key: &RequestRetryKey) -> Option<PendingRequestRetry> {
        let pending = self.pending_request_retries.remove(key)?;
        self.request_retry_by_sub.remove(&pending.attempt.sub_id);
        if let Some(keys) = self
            .request_retries_by_session
            .get_mut(&pending.attempt.session)
        {
            keys.remove(key);
            if keys.is_empty() {
                self.request_retries_by_session
                    .remove(&pending.attempt.session);
            }
        }
        Some(pending)
    }

    pub(super) fn retry_due_request_attempts(&mut self, now: Timestamp, effects: &mut Vec<Effect>) {
        let due: Vec<_> = self
            .pending_request_retries
            .iter()
            .filter(|(_, pending)| pending.due <= now)
            .map(|(key, _)| key.clone())
            .collect();
        for key in due {
            let Some(pending) = self.remove_request_retry(&key) else {
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
            self.request_attempts
                .get_mut(&attempt_id)
                .expect("the retry dispatch just minted its exact attempt")
                .retry_failures = pending.failures;
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
