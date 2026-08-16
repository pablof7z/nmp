//! The NIP-02 following observation and typed follow/unfollow action.
//!
//! Lives beside [`super::writes`] for the same reason: both need the engine
//! itself (`Arc<Engine>`, `WriteIntent`/receipt custody), which
//! `nmp-nip02`'s pure reactive-query vocabulary cannot depend on without
//! reversing the package graph (#1143).

use std::collections::BTreeMap;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use nmp_nip02::current_account_demand;
use nostr::PublicKey;

use crate::{
    AcquisitionEvidence, Engine, EventId, LiveQuery, ObservationCancel, ReceiptStream, Row,
    RowDelta, ShortfallFact, SourceStatus,
};

use super::writes::{follows, FollowChange, FollowWrites};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowRelationship {
    Unknown,
    NotFollowing,
    Following,
}

/// Source evidence for the live relationship projection. This does not gate
/// the semantic action: cached state and first-value creation remain writable
/// while relay truth is incomplete. `Ready` deliberately does not claim
/// global Nostr completeness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowAvailability {
    SignedOut,
    Acquiring,
    Ready,
    NoContactList,
    CachedOnly,
    SourceUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowSnapshot {
    pub current_pubkey: Option<PublicKey>,
    pub target: PublicKey,
    pub relationship: FollowRelationship,
    pub availability: FollowAvailability,
    pub base_event_id: Option<EventId>,
}

/// Why a typed follow/unfollow action was refused before ordinary receipt
/// custody. `EngineClosed` and `PublishRefused` name exactly what
/// [`crate::Engine::publish`] itself can return for this call
/// ([`crate::EngineError`] has no other reachable variant here); there is no
/// separate follow-only fiction standing in for a receipt that failed to
/// materialize for no named reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FollowActionFailure {
    SignedOut,
    EngineClosed,
    PublishRefused { reason: String },
}

impl std::fmt::Display for FollowActionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SignedOut => f.write_str("no current account is selected"),
            Self::EngineClosed => f.write_str("the engine is closed"),
            Self::PublishRefused { reason } => write!(f, "{reason}"),
        }
    }
}

impl std::error::Error for FollowActionFailure {}

#[derive(Default)]
struct Accumulator {
    rows: BTreeMap<EventId, Row>,
}

impl Accumulator {
    fn apply(&mut self, deltas: Vec<RowDelta>) {
        for delta in deltas {
            match delta {
                RowDelta::Added(row) => {
                    self.rows.insert(row.id(), row);
                }
                RowDelta::Updated(row) => {
                    self.rows.insert(row.id(), row);
                }
                RowDelta::Removed(id) => {
                    self.rows.remove(&id);
                }
                RowDelta::SourcesGrew { .. } => {}
            }
        }
    }

    fn base_for(&self, current: PublicKey) -> Option<&Row> {
        self.rows
            .values()
            .find(|row| row.pubkey() == current && row.kind() == nostr::Kind::ContactList)
    }
}

/// This service observes ONE branch, but reads its frame's per-branch
/// evidence as the slice it is: a branch that reports a hard shortfall or a
/// failed source makes the whole projection unavailable, and every branch
/// must have proven something before it reads Ready. No branch's proof ever
/// stands in for another's.
fn availability(
    current: Option<PublicKey>,
    evidence: &[AcquisitionEvidence],
) -> FollowAvailability {
    if current.is_none() {
        return FollowAvailability::SignedOut;
    }

    let shortfall = || evidence.iter().flat_map(|branch| branch.shortfall.iter());
    let sources = || evidence.iter().flat_map(|branch| branch.sources.iter());

    let hard_shortfall = shortfall().any(|fact| {
        matches!(
            fact,
            ShortfallFact::NoPlannedSource { .. } | ShortfallFact::LocalLimit { .. }
        )
    });
    let hard_source_failure = sources().any(|source| {
        matches!(
            source.status,
            SourceStatus::AuthDenied | SourceStatus::Error
        )
    });
    if hard_shortfall || hard_source_failure {
        return FollowAvailability::SourceUnavailable;
    }

    if sources().next().is_none() || sources().any(|source| source.reconciled_through.is_none()) {
        return FollowAvailability::Acquiring;
    }

    if sources().any(|source| source.status == SourceStatus::Disconnected) {
        return FollowAvailability::CachedOnly;
    }

    // `Requesting` and `FinishedStoredEvents` are the two connected-and-live
    // states (#1235); `Ready` is about the link plus the watermark, not about
    // how far the current request has got.
    if sources().all(|source| {
        matches!(
            source.status,
            SourceStatus::Requesting | SourceStatus::FinishedStoredEvents
        ) && source.reconciled_through.is_some()
    }) && shortfall().next().is_none()
    {
        FollowAvailability::Ready
    } else {
        FollowAvailability::Acquiring
    }
}

fn project(
    current: Option<PublicKey>,
    target: PublicKey,
    accumulator: &Accumulator,
    evidence: &[AcquisitionEvidence],
) -> FollowSnapshot {
    let evidence_availability = availability(current, evidence);
    let base = current.and_then(|pubkey| accumulator.base_for(pubkey));
    let availability = if current.is_some()
        && base.is_none()
        && evidence_availability == FollowAvailability::Ready
    {
        FollowAvailability::NoContactList
    } else {
        evidence_availability
    };
    let relationship = match base {
        Some(base) if follows(base, target) => FollowRelationship::Following,
        Some(_) => FollowRelationship::NotFollowing,
        None if availability == FollowAvailability::NoContactList => {
            FollowRelationship::NotFollowing
        }
        None => FollowRelationship::Unknown,
    };
    FollowSnapshot {
        current_pubkey: current,
        target,
        relationship,
        availability,
        base_event_id: base.map(Row::id),
    }
}

#[derive(Default)]
struct LatestState {
    value: Option<FollowSnapshot>,
    closed: bool,
}

#[derive(Default)]
struct LatestSlot {
    state: Mutex<LatestState>,
    changed: Condvar,
}

impl LatestSlot {
    fn send(&self, value: FollowSnapshot) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.value = Some(value);
        self.changed.notify_one();
    }

    fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.closed = true;
        self.changed.notify_all();
    }

    fn recv(&self) -> Option<FollowSnapshot> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        loop {
            if let Some(value) = state.value.take() {
                return Some(value);
            }
            if state.closed {
                return None;
            }
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(|poison| poison.into_inner());
        }
    }

    fn recv_timeout(&self, timeout: Duration) -> Result<FollowSnapshot, RecvTimeoutError> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (mut state, wait) = self
            .changed
            .wait_timeout_while(state, timeout, |state| {
                state.value.is_none() && !state.closed
            })
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(value) = state.value.take() {
            return Ok(value);
        }
        if state.closed {
            return Err(RecvTimeoutError::Disconnected);
        }
        debug_assert!(wait.timed_out());
        Err(RecvTimeoutError::Timeout)
    }
}

/// A latest-wins, bounded projection over one ordinary NMP live query.
/// Dropping it withdraws demand; no component-level claim/release registry
/// exists.
pub struct FollowObservation {
    cancel: ObservationCancel,
    latest: Arc<LatestSlot>,
}

impl FollowObservation {
    pub fn recv(&self) -> Option<FollowSnapshot> {
        self.latest.recv()
    }

    /// Wait at most `timeout` for the next latest-wins relationship
    /// snapshot. Timeout and engine/demand teardown remain distinct.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<FollowSnapshot, RecvTimeoutError> {
        self.latest.recv_timeout(timeout)
    }

    pub fn cancel_handle(&self) -> ObservationCancel {
        self.cancel.clone()
    }
}

impl Drop for FollowObservation {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// #704: the latest-wins follow observer is an async task on the engine
/// runtime (no dedicated OS thread). It drains the waker-driven async row
/// mailbox and folds each frame into the latest slot; the blocking
/// `FollowObservation::recv`/`recv_timeout` consumer reads that slot. There is
/// no admission slot to reserve — nothing is refused.
pub fn observe_following(
    engine: Arc<Engine>,
    target: PublicKey,
) -> Result<FollowObservation, crate::EngineError> {
    let runtime = engine.adapter_runtime()?;
    let subscription = engine.observe_async(LiveQuery::single(current_account_demand()), None)?;
    let cancel = subscription.cancel_handle();
    let latest = Arc::new(LatestSlot::default());
    let producer = latest.clone();
    runtime.spawn(async move {
        let mut accumulator = Accumulator::default();
        while let Ok(Some(frame)) = subscription.next().await {
            accumulator.apply(frame.deltas);
            let current = engine
                .session()
                .ok()
                .and_then(|session| session.current_pubkey);
            producer.send(project(current, target, &accumulator, &frame.evidence));
        }
        producer.close();
    });
    Ok(FollowObservation { cancel, latest })
}

/// The pull-based async twin of [`FollowObservation`] (#680). Instead of a
/// dedicated worker thread draining a blocking subscription into a latest-slot
/// (one native thread per follow observation — the defect), this projects
/// inline when the consumer awaits [`Self::next`]: the relationship snapshot is
/// derived from the folded accumulator the moment a row frame is pulled. The
/// projection is a complete self-contained snapshot, so a lost/redelivered
/// frame under per-call cancellation is benign.
pub struct AsyncFollowObservation {
    subscription: crate::AsyncSubscription,
    engine: Arc<Engine>,
    target: PublicKey,
    accumulator: Mutex<Accumulator>,
}

impl AsyncFollowObservation {
    /// Await the next relationship snapshot, or `None` once the underlying
    /// demand is withdrawn. [`crate::ConcurrentNext`] on an overlapping call.
    pub async fn next(&self) -> Result<Option<FollowSnapshot>, crate::ConcurrentNext> {
        match self.subscription.next().await? {
            Some(frame) => {
                let mut accumulator = self.accumulator.lock().unwrap();
                accumulator.apply(frame.deltas);
                let current = self
                    .engine
                    .session()
                    .ok()
                    .and_then(|session| session.current_pubkey);
                Ok(Some(project(
                    current,
                    self.target,
                    &accumulator,
                    &frame.evidence,
                )))
            }
            None => Ok(None),
        }
    }

    /// Withdraw the observation now (idempotent; `Drop` does the same).
    pub fn cancel(&self) {
        self.subscription.cancel();
    }

    pub fn cancel_handle(&self) -> ObservationCancel {
        self.subscription.cancel_handle()
    }
}

/// Open a follow observation delivered by awaiting `next()` (#680). Costs no
/// native thread: the projection folds inline in `next()` over the engine's
/// waker-driven async row mailbox.
pub fn observe_following_async(
    engine: Arc<Engine>,
    target: PublicKey,
) -> Result<AsyncFollowObservation, crate::EngineError> {
    let subscription = engine.observe_async(LiveQuery::single(current_account_demand()), None)?;
    Ok(AsyncFollowObservation {
        subscription,
        engine,
        target,
        accumulator: Mutex::new(Accumulator::default()),
    })
}

/// Apply one typed NIP-02 change through the ordinary durable semantic-write
/// path and return its ordinary receipt.
///
/// This call does not acquire a relay-ready base or start a second action
/// worker. It freezes the active account before capability execution, then
/// uses an explicit identity so a later account switch cannot retarget the
/// accepted operation. NMP selects the best canonical source it already has;
/// when none exists, NIP-02 supplies its complete empty kind-3 value.
pub fn set_following(
    engine: &Engine,
    writes: &FollowWrites,
    target: PublicKey,
    change: FollowChange,
) -> Result<ReceiptStream, FollowActionFailure> {
    let author = match engine.session() {
        Ok(session) => session
            .current_pubkey
            .ok_or(FollowActionFailure::SignedOut)?,
        Err(_) => return Err(FollowActionFailure::EngineClosed),
    };
    let intent = writes.intent(author, target, change);
    engine.publish(intent).map_err(|error| match error {
        crate::EngineError::EngineClosed => FollowActionFailure::EngineClosed,
        other => FollowActionFailure::PublishRefused {
            reason: other.to_string(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AccessContext, EngineConfig, RelayUrl, SigningState, SourceEvidence, WriteFact};
    use nostr::Keys;

    #[test]
    fn signed_out_action_fails_typed_without_a_write() {
        let engine = Engine::new_with_capabilities(
            EngineConfig::default(),
            vec![crate::nip02::follow_capability()],
        )
        .unwrap();
        let writes = crate::nip02::follow_writes();
        let failure = set_following(
            &engine,
            &writes,
            Keys::generate().public_key(),
            FollowChange::Follow,
        )
        .err();
        assert_eq!(failure, Some(FollowActionFailure::SignedOut));
        assert!(engine.publish_queue(None, 10).unwrap().is_empty());
        engine.shutdown();
    }

    #[test]
    fn logged_in_without_sources_accepts_the_capability_default() {
        let engine = Engine::new_with_capabilities(
            EngineConfig::default(),
            vec![crate::nip02::follow_capability()],
        )
        .unwrap();
        let author = Keys::generate();
        engine
            .add_private_key_account(&author.secret_key().to_secret_bytes(), true)
            .unwrap();
        let writes = crate::nip02::follow_writes();
        let receipt = set_following(
            &engine,
            &writes,
            Keys::generate().public_key(),
            FollowChange::Follow,
        )
        .expect("the NIP-02 empty contact list enters ordinary custody");
        assert_eq!(engine.publish_queue(None, 10).unwrap().len(), 1);
        assert_eq!(
            engine.publish_queue(None, 10).unwrap()[0].receipt_id,
            receipt.id
        );
        engine.shutdown();
    }

    #[test]
    fn account_switch_after_action_cannot_retarget_the_frozen_author() {
        let engine = Engine::new_with_capabilities(
            EngineConfig::default(),
            vec![crate::nip02::follow_capability()],
        )
        .unwrap();
        let author = Keys::generate().public_key();
        let later_account = Keys::generate().public_key();
        engine.add_public_key_account(author, true).unwrap();
        engine.add_public_key_account(later_account, false).unwrap();
        let writes = crate::nip02::follow_writes();

        let receipt = set_following(
            &engine,
            &writes,
            Keys::generate().public_key(),
            FollowChange::Follow,
        )
        .expect("the action enters custody under the selected author");
        engine.make_current_account(later_account).unwrap();

        assert_eq!(
            receipt.statuses.recv_timeout(Duration::from_secs(5)),
            Ok(WriteFact::Signing(SigningState::AwaitingSigner {
                pubkey: author,
            })),
            "the receipt must remain bound to the account selected before custody"
        );
        engine.shutdown();
    }

    #[test]
    fn reconciled_absence_remains_observation_truth_not_source_provenance() {
        let author = Keys::generate().public_key();
        let target = Keys::generate().public_key();
        let evidence = AcquisitionEvidence {
            sources: vec![SourceEvidence {
                relay: RelayUrl::parse("wss://relay.example").unwrap(),
                access: AccessContext::Public,
                reconciled_through: Some(nostr::Timestamp::from_secs(10)),
                status: SourceStatus::Requesting,
            }],
            shortfall: vec![],
        };

        let snapshot = project(
            Some(author),
            target,
            &Accumulator::default(),
            std::slice::from_ref(&evidence),
        );
        assert_eq!(snapshot.relationship, FollowRelationship::NotFollowing);
        assert_eq!(snapshot.availability, FollowAvailability::NoContactList);
        assert_eq!(snapshot.base_event_id, None);
    }
}
