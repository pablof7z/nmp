//! Typed ownership for one exact local request-send attempt (#849/#774).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use nmp_grammar::{ConcreteFilter, DescriptorHash, RelaySessionKey};
use nmp_router::{SubId, WireDelta, WireOp};
use nmp_store::CoverageKey;
use nostr::Timestamp;

use super::{
    bootstrap_retry_delay_secs, CoreState, Effect, EventFailureTarget, TransportRelayHandle,
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
/// only this file can. Privacy alone only makes the reverse-index invariants
/// below ENFORCEABLE rather than every caller remembering them (#1606); what
/// actually enforces them is the asserts in `remove`/`remove_retry` and the
/// owner-scoped bulk removals that call them, checked structurally by
/// `assert_consistent`.
///
/// It holds state and the invariants over that state, and nothing else: no
/// `store`, no `router`, no `resolver`, no `Effect`. Anything that has to
/// emit is orchestration and stays on `CoreState`. This is the
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

    /// Retire every attempt this sub owns.
    ///
    /// This owns the `by_sub` bucket wholesale, so the per-attempt cleanup
    /// below must never re-probe `by_sub` for an id it just tore the bucket
    /// out from under -- that used to be exactly the disagreement `remove`
    /// tolerated silently. Only the OTHER mirror (`by_session`) is still
    /// live for each attempt, so only it gets forgotten here, and forgetting
    /// it panics rather than tolerates absence.
    pub(super) fn retire_for_sub(&mut self, sub_id: &SubId) {
        let attempt_ids = self.by_sub.remove(sub_id).unwrap_or_default();
        for attempt_id in attempt_ids {
            let attempt = self.attempts.remove(&attempt_id).unwrap_or_else(|| {
                panic!("RequestAttempts: by_sub named attempt {attempt_id:?}, which is not live")
            });
            self.forget_session_edge(attempt_id, &attempt.session);
        }
        self.cancel_retry_for_sub(sub_id);
    }

    /// Retire every attempt and every parked retry this session owns.
    ///
    /// Same shape as `retire_for_sub`, mirrored: `by_session` and
    /// `retries_by_session` are torn out wholesale first, so only the
    /// `by_sub` / `retry_by_sub` mirrors are still live per element and are
    /// the only ones forgotten below.
    pub(super) fn retire_for_session(&mut self, session: &RelaySessionKey) {
        let attempt_ids = self.by_session.remove(session).unwrap_or_default();
        for attempt_id in attempt_ids {
            let attempt = self.attempts.remove(&attempt_id).unwrap_or_else(|| {
                panic!(
                    "RequestAttempts: by_session named attempt {attempt_id:?}, which is not live"
                )
            });
            self.forget_sub_edge(attempt_id, &attempt.sub_id);
        }
        let retry_keys = self.retries_by_session.remove(session).unwrap_or_default();
        for key in retry_keys {
            let pending = self.retries.remove(&key).unwrap_or_else(|| {
                panic!("RequestAttempts: retries_by_session named retry {key:?}, which is not live")
            });
            self.forget_retry_sub_edge(&pending.attempt.sub_id, &key);
        }
    }

    /// Remove one attempt id from BOTH reverse indexes. Used only where
    /// neither mirror has already been torn out for this id (the solo
    /// removal path); the owner-scoped bulk paths above forget exactly one
    /// mirror each, because the other one is already gone.
    fn remove(&mut self, attempt_id: RequestAttemptId) -> Option<RequestAttemptState> {
        let attempt = self.attempts.remove(&attempt_id)?;
        self.forget_sub_edge(attempt_id, &attempt.sub_id);
        self.forget_session_edge(attempt_id, &attempt.session);
        Some(attempt)
    }

    /// Remove `attempt_id` from its `by_sub` bucket. Absent is not a valid
    /// answer once the forward map said the attempt existed under this sub
    /// -- that disagreement is exactly what silent-tolerance used to hide.
    fn forget_sub_edge(&mut self, attempt_id: RequestAttemptId, sub_id: &SubId) {
        let ids = self.by_sub.get_mut(sub_id).unwrap_or_else(|| {
            panic!("RequestAttempts: a live attempt's sub has no by_sub reverse set")
        });
        assert!(
            ids.remove(&attempt_id),
            "RequestAttempts: a live attempt's sub did not name it in by_sub"
        );
        if ids.is_empty() {
            self.by_sub.remove(sub_id);
        }
    }

    /// `forget_sub_edge`'s twin for `by_session`.
    fn forget_session_edge(&mut self, attempt_id: RequestAttemptId, session: &RelaySessionKey) {
        let ids = self.by_session.get_mut(session).unwrap_or_else(|| {
            panic!("RequestAttempts: a live attempt's session has no by_session reverse set")
        });
        assert!(
            ids.remove(&attempt_id),
            "RequestAttempts: a live attempt's session did not name it in by_session"
        );
        if ids.is_empty() {
            self.by_session.remove(session);
        }
    }

    /// `now` is an argument rather than a read of `CoreState::clock`: the
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

    /// Cancel the retry parked behind `sub_id`, if any. `retry_by_sub` is
    /// torn out wholesale first, so only the OTHER mirror
    /// (`retries_by_session`) is still live for it -- same shape as
    /// `retire_for_sub`, one owner smaller.
    pub(super) fn cancel_retry_for_sub(&mut self, sub_id: &SubId) {
        let Some(key) = self.retry_by_sub.remove(sub_id) else {
            return;
        };
        let pending = self.retries.remove(&key).unwrap_or_else(|| {
            panic!("RequestAttempts: retry_by_sub named retry {key:?}, which is not live")
        });
        self.forget_retry_session_edge(&pending.attempt.session, &key);
    }

    /// Remove one retry from BOTH reverse indexes. Used only where neither
    /// mirror has already been torn out for this key.
    fn remove_retry(&mut self, key: &RequestRetryKey) -> Option<PendingRequestRetry> {
        let pending = self.retries.remove(key)?;
        self.forget_retry_sub_edge(&pending.attempt.sub_id, key);
        self.forget_retry_session_edge(&pending.attempt.session, key);
        Some(pending)
    }

    /// Remove `sub_id`'s `retry_by_sub` entry, asserting it named exactly
    /// `key`. At most one retry is ever parked per sub (`schedule_retry`
    /// clears any prior entry under the same `RequestRetryKey` before
    /// inserting), so a live retry's sub either names it or the mirror has
    /// already diverged.
    fn forget_retry_sub_edge(&mut self, sub_id: &SubId, key: &RequestRetryKey) {
        let mapped = self.retry_by_sub.remove(sub_id).unwrap_or_else(|| {
            panic!("RequestAttempts: a live retry's sub has no retry_by_sub entry")
        });
        assert_eq!(
            &mapped, key,
            "RequestAttempts: retry_by_sub for this sub named a different retry"
        );
    }

    /// `forget_retry_sub_edge`'s twin for `retries_by_session`.
    fn forget_retry_session_edge(&mut self, session: &RelaySessionKey, key: &RequestRetryKey) {
        let keys = self.retries_by_session.get_mut(session).unwrap_or_else(|| {
            panic!("RequestAttempts: a live retry's session has no retries_by_session reverse set")
        });
        assert!(
            keys.remove(key),
            "RequestAttempts: a live retry's session did not name it in retries_by_session"
        );
        if keys.is_empty() {
            self.retries_by_session.remove(session);
        }
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

    /// Exact structural consistency for every mirror this owner keeps, by
    /// identity rather than by count.
    ///
    /// `counts()` next to this counts things -- the right instrument for
    /// leaks and boundedness, and the wrong one for structure: an attempt
    /// indexed under the wrong sub, or a retry under the wrong session,
    /// preserves every number `counts()` reports (`OwnerIndexed`'s own
    /// `assert_consistent` doc makes the same point). Four mirrors are
    /// checked in both directions: `by_sub`, `by_session`,
    /// `retry_by_sub`, `retries_by_session`.
    #[cfg(any(test, feature = "bench-instrumentation"))]
    pub(super) fn assert_consistent(&self, at: &str) {
        for (attempt_id, attempt) in &self.attempts {
            let ids = self.by_sub.get(&attempt.sub_id).unwrap_or_else(|| {
                panic!(
                    "{at}: request attempt {attempt_id:?} has no by_sub reverse set for its own sub {:?}",
                    attempt.sub_id
                )
            });
            assert!(
                ids.contains(attempt_id),
                "{at}: request attempt {attempt_id:?} is not named by its own sub's reverse index"
            );
            let ids = self.by_session.get(&attempt.session).unwrap_or_else(|| {
                panic!(
                    "{at}: request attempt {attempt_id:?} has no by_session reverse set for its own session"
                )
            });
            assert!(
                ids.contains(attempt_id),
                "{at}: request attempt {attempt_id:?} is not named by its own session's reverse index"
            );
        }
        for (sub_id, ids) in &self.by_sub {
            assert!(
                !ids.is_empty(),
                "{at}: request attempts kept an empty by_sub reverse set for sub {sub_id:?}"
            );
            for attempt_id in ids {
                let attempt = self.attempts.get(attempt_id).unwrap_or_else(|| {
                    panic!("{at}: by_sub names attempt {attempt_id:?}, which is not live")
                });
                assert_eq!(
                    &attempt.sub_id, sub_id,
                    "{at}: attempt {attempt_id:?} is indexed under a sub it does not report"
                );
            }
        }
        for (session, ids) in &self.by_session {
            assert!(
                !ids.is_empty(),
                "{at}: request attempts kept an empty by_session reverse set for session {session:?}"
            );
            for attempt_id in ids {
                let attempt = self.attempts.get(attempt_id).unwrap_or_else(|| {
                    panic!("{at}: by_session names attempt {attempt_id:?}, which is not live")
                });
                assert_eq!(
                    &attempt.session, session,
                    "{at}: attempt {attempt_id:?} is indexed under a session it does not report"
                );
            }
        }
        for (key, pending) in &self.retries {
            let sub_id = &pending.attempt.sub_id;
            let mapped = self.retry_by_sub.get(sub_id).unwrap_or_else(|| {
                panic!("{at}: retry {key:?} has no retry_by_sub entry for its own sub {sub_id:?}")
            });
            assert_eq!(
                mapped, key,
                "{at}: retry_by_sub for sub {sub_id:?} does not name retry {key:?}"
            );
            let session = &pending.attempt.session;
            let keys = self.retries_by_session.get(session).unwrap_or_else(|| {
                panic!(
                    "{at}: retry {key:?} has no retries_by_session reverse set for its own session"
                )
            });
            assert!(
                keys.contains(key),
                "{at}: retry {key:?} is not named by its own session's reverse index"
            );
        }
        for (sub_id, key) in &self.retry_by_sub {
            let pending = self.retries.get(key).unwrap_or_else(|| {
                panic!(
                    "{at}: retry_by_sub names retry {key:?} for sub {sub_id:?}, which is not live"
                )
            });
            assert_eq!(
                &pending.attempt.sub_id, sub_id,
                "{at}: retry {key:?} is indexed under a sub it does not report"
            );
        }
        for (session, keys) in &self.retries_by_session {
            assert!(
                !keys.is_empty(),
                "{at}: request attempts kept an empty retries_by_session reverse set for session {session:?}"
            );
            for key in keys {
                let pending = self.retries.get(key).unwrap_or_else(|| {
                    panic!("{at}: retries_by_session names retry {key:?}, which is not live")
                });
                assert_eq!(
                    &pending.attempt.session, session,
                    "{at}: retry {key:?} is indexed under a session it does not report"
                );
            }
        }
    }
}

impl CoreState {
    /// Which role subscriptions one plan's metadata update applies to.
    ///
    /// The NIP-77 fan-out the attempt owner deliberately cannot see. It used
    /// to be computed here by reaching into four of the repair owner's maps;
    /// it now comes from that owner directly, as the comment here always
    /// said it eventually would.
    fn role_sub_ids_for_plan(&self, plan_sub_id: &SubId) -> BTreeSet<SubId> {
        self.nip77.role_sub_ids_for_plan(plan_sub_id)
    }

    pub(in crate::core) fn extend_request_attempt_metadata(
        &mut self,
        update: &nmp_router::RequestMetadataUpdate,
    ) {
        let role_sub_ids = self.role_sub_ids_for_plan(&update.sub_id);
        self.attempts.extend_metadata(&role_sub_ids, update);
    }

    pub(in crate::core) fn remove_request_attempt_metadata(
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
    pub(in crate::core) fn retry_due_request_attempts(
        &mut self,
        now: Timestamp,
        effects: &mut Vec<Effect>,
    ) {
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
            RequestAttemptPurpose::Nip77LiveCandidate { plan_sub_id } => {
                self.nip77.is_handoff_child_of(plan_sub_id, &attempt.sub_id)
            }
            RequestAttemptPurpose::Nip77MissingIds { plan_sub_id }
            | RequestAttemptPurpose::Nip77Backlog { plan_sub_id } => self
                .nip77
                .is_backfill_child_of(plan_sub_id, &attempt.sub_id),
            RequestAttemptPurpose::Nip77Open { .. }
            | RequestAttemptPurpose::Nip77Probe
            | RequestAttemptPurpose::Nip77Continue => false,
        }
    }
}

/// This owner's own falsifiers, in the same spirit as
/// `owner_index::tests::take_owner_panics_on_a_reverse_edge_the_forward_map_already_lost`:
/// two reach past every public method to corrupt a mirror by hand (only
/// possible from inside this module, since the maps are private to it), and
/// two drive the real `pub(super)` surface `CoreState` itself calls.
#[cfg(test)]
mod tests {
    use nmp_grammar::{AccessContext, SourceAuthority};
    use nostr::RelayUrl;

    use super::*;

    fn filter(kind: u16) -> ConcreteFilter {
        ConcreteFilter {
            kinds: Some(BTreeSet::from([kind])),
            ..ConcreteFilter::default()
        }
    }

    /// A minimal, valid `Ordinary` attempt on `relay`, distinguished from any
    /// other by its filter's kind (so two calls with different `kind`s on the
    /// same relay mint distinct `SubId`s).
    fn attempt(relay: &RelayUrl, kind: u16) -> (RelaySessionKey, SubId, RequestAttemptState) {
        let session = RelaySessionKey::public(relay.clone());
        let filter = filter(kind);
        let filter_hash = filter.hash();
        let sub_id = SubId::for_wire(
            relay.clone(),
            &filter,
            &SourceAuthority::Public,
            AccessContext::Public,
        );
        let state = RequestAttemptState {
            session: session.clone(),
            sub_id: sub_id.clone(),
            filter_hash,
            filter,
            coverage_claims: BTreeSet::new(),
            owner_demands: BTreeSet::new(),
            replay: false,
            event_failure_target: EventFailureTarget::ThisSend,
            request_revision: None,
            retry_failures: 0,
            purpose: RequestAttemptPurpose::Ordinary,
        };
        (session, sub_id, state)
    }

    /// The exact disagreement `remove` used to tolerate silently: the
    /// forward map (`attempts`) and the OTHER mirror (`by_session`) both
    /// still name a live attempt, but its `by_sub` bucket has already been
    /// torn out from under it. Before this change `remove` skipped `by_sub`
    /// via `if let` and returned the attempt as if nothing were wrong; now
    /// it panics naming exactly the mirror that disagreed.
    #[test]
    #[should_panic(expected = "a live attempt's sub has no by_sub reverse set")]
    fn remove_panics_when_by_sub_mirror_already_disagrees() {
        let mut attempts = RequestAttempts::new();
        let relay = RelayUrl::parse("wss://remove-disagree.example").expect("valid url");
        let (_, _, state) = attempt(&relay, 1);
        let id = attempts.mint(state);

        // Precondition: the mirror is intact before corrupting it.
        assert_eq!(
            attempts.by_sub.values().map(BTreeSet::len).sum::<usize>(),
            1
        );
        assert!(attempts.attempts.contains_key(&id));

        // Corrupt only `by_sub`, bypassing every real removal path. `by_session`
        // and `attempts` still agree the attempt is live.
        attempts.by_sub.clear();

        let _ = attempts.remove(id);
    }

    /// `assert_consistent`'s falsifier: swap which attempt each of two subs'
    /// `by_sub` buckets names, WITHOUT changing key count (2) or edge count
    /// (2). A census that only counts keys and edges cannot see this --
    /// that is the whole point of checking identity instead.
    #[test]
    #[should_panic(expected = "is not named by its own sub's reverse index")]
    fn assert_consistent_catches_a_cardinality_preserving_owner_swap() {
        let mut attempts = RequestAttempts::new();
        let relay_a = RelayUrl::parse("wss://swap-a.example").expect("valid url");
        let relay_b = RelayUrl::parse("wss://swap-b.example").expect("valid url");
        let (_, sub_a, state_a) = attempt(&relay_a, 1);
        let (_, sub_b, state_b) = attempt(&relay_b, 1);
        let id_a = attempts.mint(state_a);
        let id_b = attempts.mint(state_b);

        // Precondition: the mirror is intact and each attempt is under its
        // own reported sub.
        attempts.assert_consistent("precondition");

        // Swap membership between the two owners' buckets. Total sub keys
        // (2) and total edges (2) are unchanged -- only identity moved.
        attempts
            .by_sub
            .get_mut(&sub_a)
            .expect("sub_a bucket")
            .clear();
        attempts
            .by_sub
            .get_mut(&sub_a)
            .expect("sub_a bucket")
            .insert(id_b);
        attempts
            .by_sub
            .get_mut(&sub_b)
            .expect("sub_b bucket")
            .clear();
        attempts
            .by_sub
            .get_mut(&sub_b)
            .expect("sub_b bucket")
            .insert(id_a);
        assert_eq!(
            attempts.by_sub.len(),
            2,
            "owner-key count must be unchanged"
        );
        assert_eq!(
            attempts.by_sub.values().map(BTreeSet::len).sum::<usize>(),
            2,
            "edge count must be unchanged"
        );

        attempts.assert_consistent("after swap");
    }

    /// The real behaviour, driven entirely through the same `pub(super)`
    /// surface `CoreState` calls: retiring one sub removes only that sub's
    /// attempt and its parked retry, leaves the other sub's attempt live,
    /// prunes the vacated `by_sub` bucket, and leaves the shared session's
    /// `by_session` bucket correctly shrunk rather than destroyed.
    #[test]
    fn retire_for_sub_removes_only_that_subs_attempt_and_retry() {
        let mut attempts = RequestAttempts::new();
        let relay = RelayUrl::parse("wss://retire-sub.example").expect("valid url");
        let (_session, sub_a, state_a) = attempt(&relay, 1);
        let (_, _sub_b, state_b) = attempt(&relay, 2);
        let id_a = attempts.mint(state_a.clone());
        let id_b = attempts.mint(state_b);
        attempts.schedule_retry(state_a, Timestamp::from(0u64));

        let before = attempts.counts();
        assert_eq!(before.attempts, 2);
        assert_eq!(before.sub_keys, 2);
        assert_eq!(before.session_keys, 1);
        assert_eq!(before.session_edges, 2);
        assert_eq!(before.retry_jobs, 1);
        attempts.assert_consistent("before retire_for_sub");

        attempts.retire_for_sub(&sub_a);

        assert!(attempts.get(id_a).is_none(), "sub_a's attempt is gone");
        assert!(attempts.get(id_b).is_some(), "sub_b's attempt survives");
        let after = attempts.counts();
        assert_eq!(after.attempts, 1);
        assert_eq!(after.sub_keys, 1, "sub_a's now-empty bucket is pruned");
        assert_eq!(after.session_keys, 1, "session still holds sub_b's attempt");
        assert_eq!(after.session_edges, 1);
        assert_eq!(after.retry_jobs, 0, "sub_a's retry is cancelled with it");
        assert_eq!(after.retry_sub_keys, 0);
        assert_eq!(after.retry_session_keys, 0);
        attempts.assert_consistent("after retire_for_sub");
    }

    /// `retire_for_session`'s twin proof: retiring the session removes every
    /// attempt and every parked retry it owns, across different subs, and
    /// leaves no dangling `by_sub`/`retry_by_sub` mirrors behind.
    #[test]
    fn retire_for_session_removes_every_attempt_and_retry_it_owns() {
        let mut attempts = RequestAttempts::new();
        let relay = RelayUrl::parse("wss://retire-session.example").expect("valid url");
        let (session, _sub_a, state_a) = attempt(&relay, 1);
        let (_, _sub_b, state_b) = attempt(&relay, 2);
        let id_a = attempts.mint(state_a.clone());
        let id_b = attempts.mint(state_b.clone());
        attempts.schedule_retry(state_a, Timestamp::from(0u64));
        attempts.schedule_retry(state_b, Timestamp::from(0u64));

        let before = attempts.counts();
        assert_eq!(before.attempts, 2);
        assert_eq!(before.retry_jobs, 2);
        attempts.assert_consistent("before retire_for_session");

        attempts.retire_for_session(&session);

        assert!(attempts.get(id_a).is_none());
        assert!(attempts.get(id_b).is_none());
        let after = attempts.counts();
        assert_eq!(after.attempts, 0);
        assert_eq!(after.sub_keys, 0);
        assert_eq!(after.session_keys, 0);
        assert_eq!(after.retry_jobs, 0);
        assert_eq!(after.retry_sub_keys, 0);
        assert_eq!(after.retry_session_keys, 0);
        attempts.assert_consistent("after retire_for_session");
    }
}
