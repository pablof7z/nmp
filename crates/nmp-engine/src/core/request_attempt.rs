//! Typed ownership for one exact local request-send attempt (#849/#774).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use nmp_grammar::{ConcreteFilter, DescriptorHash, RelaySessionKey};
use nmp_router::{SubId, WireDelta, WireOp};
use nmp_store::CoverageKey;
use nostr::Timestamp;

use super::{
    unjittered_retry_delay_secs, CoreState, Effect, TransportRelayHandle,
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

#[derive(Debug, Clone)]
pub(super) struct RequestAttemptState {
    pub(super) session: RelaySessionKey,
    pub(super) sub_id: SubId,
    pub(super) filter_hash: DescriptorHash,
    pub(super) filter: ConcreteFilter,
    pub(super) coverage_claims: std::collections::BTreeSet<CoverageKey>,
    pub(super) owner_demands: std::collections::BTreeSet<nmp_router::DemandKey>,
    pub(super) request_revision: Option<u64>,
    /// Refusals already observed for this one semantic retry goal.
    /// Carried through Attempting so backoff never resets when the retry
    /// record leaves the deadline map for dispatch.
    pub(super) retry_failures: u32,
}

pub(super) struct RequestSend<'a> {
    pub(super) session: &'a RelaySessionKey,
    pub(super) sub_id: &'a SubId,
    pub(super) filter: &'a ConcreteFilter,
    pub(super) coverage_claims: BTreeSet<CoverageKey>,
    pub(super) owner_demands: BTreeSet<nmp_router::DemandKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct RequestRetryKey(pub(super) RelaySessionKey, pub(super) SubId);

#[derive(Debug, Clone)]
pub(super) struct PendingRequestRetry {
    pub(super) attempt: RequestAttemptState,
    pub(super) due: Timestamp,
    pub(super) failures: u32,
}

impl RequestAttemptState {
    pub(super) fn retry_key(&self) -> Option<RequestRetryKey> {
        Some(RequestRetryKey(self.session.clone(), self.sub_id.clone()))
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

#[cfg(feature = "bench-instrumentation")]
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
                due: now + unjittered_retry_delay_secs(failures),
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

    /// Every request awaiting a terminal, as the `(session, evidence sub id)`
    /// pairs acquisition evidence is keyed by.
    pub(super) fn awaiting_evidence_keys(&self) -> BTreeSet<(RelaySessionKey, SubId)> {
        self.attempts
            .values()
            .chain(self.retries.values().map(|retry| &retry.attempt))
            .map(|attempt| (attempt.session.clone(), attempt.sub_id.clone()))
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

    #[cfg(feature = "bench-instrumentation")]
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
    #[cfg(feature = "bench-instrumentation")]
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
            let attempt_id = self.record_observed_request_attempt(
                RequestSend {
                    session: &session,
                    sub_id: &sub_id,
                    filter: &filter,
                    coverage_claims: attempt.coverage_claims,
                    owner_demands: attempt.owner_demands,
                });
            self.attempts
                .set_retry_failures(attempt_id, pending.failures);
            effects.push(Effect::Wire(self.attempted_wire_delta(WireDelta {
                ops: vec![(session, vec![WireOp::Req(sub_id, filter)])],
            })));
        }
    }

    fn request_retry_is_current(&self, attempt: &RequestAttemptState) -> bool {
        self.plan_execution_metadata
            .get(&attempt.sub_id)
            .is_some_and(|metadata| metadata.filter == attempt.filter)
    }
}

