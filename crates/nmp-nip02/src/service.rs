use std::collections::BTreeMap;
use std::sync::mpsc::{self, Receiver, RecvError, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use nmp::{
    AcquisitionEvidence, Engine, Event, EventId, ObservationCancel, PublicKey, RowDelta,
    ShortfallFact, SourceStatus, Timestamp, WriteStatus,
};

use crate::demand::active_account_demand;
use crate::edit::{
    compose_follow_change, follows, ComposeFollowError, ComposeFollowResult, FollowChange,
};

const ACQUISITION_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_ACQUISITION_SNAPSHOTS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowRelationship {
    Unknown,
    NotFollowing,
    Following,
}

/// Whether a destructive whole-list edit is currently permitted by NMP's
/// closed default policy. `Ready` means every source in the current query
/// plan has source-scoped reconciliation evidence and is currently live;
/// it deliberately does not claim global Nostr completeness.
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
    pub active_pubkey: Option<PublicKey>,
    pub target: PublicKey,
    pub relationship: FollowRelationship,
    pub availability: FollowAvailability,
    pub base_event_id: Option<EventId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FollowActionFailure {
    SignedOut,
    AccountChanged,
    AcquisitionTimedOut,
    NoContactList,
    CachedOnly,
    SourceUnavailable,
    Compose(ComposeFollowError),
    EngineClosed,
    ReceiptUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FollowActionStatus {
    Acquiring,
    NoChange {
        following: bool,
    },
    Receipt {
        receipt_id: u64,
        status: WriteStatus,
    },
    Failed(FollowActionFailure),
}

#[derive(Default)]
struct Accumulator {
    rows: BTreeMap<EventId, Event>,
}

impl Accumulator {
    fn apply(&mut self, deltas: Vec<RowDelta>) {
        for delta in deltas {
            match delta {
                RowDelta::Added(row) => {
                    self.rows.insert(row.event.id, row.event);
                }
                RowDelta::Removed(id) => {
                    self.rows.remove(&id);
                }
                RowDelta::SourcesGrew { .. } => {}
            }
        }
    }

    fn base_for(&self, active: PublicKey) -> Option<&Event> {
        self.rows
            .values()
            .find(|event| event.pubkey == active && event.kind == nostr::Kind::ContactList)
    }
}

fn availability(active: Option<PublicKey>, evidence: &AcquisitionEvidence) -> FollowAvailability {
    if active.is_none() {
        return FollowAvailability::SignedOut;
    }

    let hard_shortfall = evidence.shortfall.iter().any(|fact| {
        matches!(
            fact,
            ShortfallFact::NoPlannedSource { .. } | ShortfallFact::LocalLimit { .. }
        )
    });
    let hard_source_failure = evidence.sources.iter().any(|source| {
        matches!(
            source.status,
            SourceStatus::AuthDenied | SourceStatus::Error
        )
    });
    if hard_shortfall || hard_source_failure {
        return FollowAvailability::SourceUnavailable;
    }

    if evidence.sources.is_empty()
        || evidence
            .sources
            .iter()
            .any(|source| source.reconciled_through.is_none())
    {
        return FollowAvailability::Acquiring;
    }

    if evidence
        .sources
        .iter()
        .any(|source| source.status == SourceStatus::Disconnected)
    {
        return FollowAvailability::CachedOnly;
    }

    if evidence.sources.iter().all(|source| {
        source.status == SourceStatus::Requesting && source.reconciled_through.is_some()
    }) && evidence.shortfall.is_empty()
    {
        FollowAvailability::Ready
    } else {
        FollowAvailability::Acquiring
    }
}

fn project(
    active: Option<PublicKey>,
    target: PublicKey,
    accumulator: &Accumulator,
    evidence: &AcquisitionEvidence,
) -> FollowSnapshot {
    let evidence_availability = availability(active, evidence);
    let base = active.and_then(|pubkey| accumulator.base_for(pubkey));
    let availability =
        if active.is_some() && base.is_none() && evidence_availability == FollowAvailability::Ready
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
        active_pubkey: active,
        target,
        relationship,
        availability,
        base_event_id: base.map(|event| event.id),
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

pub fn observe_following(
    engine: Arc<Engine>,
    target: PublicKey,
) -> Result<FollowObservation, nmp::EngineError> {
    let subscription = engine.observe(nmp::LiveQuery(active_account_demand()))?;
    let cancel = subscription.cancel_handle();
    let latest = Arc::new(LatestSlot::default());
    let producer = latest.clone();

    thread::spawn(move || {
        let mut accumulator = Accumulator::default();
        while let Ok((deltas, evidence)) = subscription.recv() {
            accumulator.apply(deltas);
            let active = engine.active_account().ok().flatten();
            producer.send(project(active, target, &accumulator, &evidence));
        }
        producer.close();
    });

    Ok(FollowObservation { cancel, latest })
}

pub struct FollowAction {
    statuses: Receiver<FollowActionStatus>,
}

impl FollowAction {
    pub fn recv(&self) -> Result<FollowActionStatus, RecvError> {
        self.statuses.recv()
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<FollowActionStatus, RecvTimeoutError> {
        self.statuses.recv_timeout(timeout)
    }
}

/// Start NMP's simple NIP-02 action. The acquisition/readiness policy,
/// exact-base edit, atomic conflict guard, signer, durable outbox routing,
/// and receipt stream all remain in Rust. A UI merely observes these states
/// and asks for `Follow` or `Unfollow`. Initial acquisition is bounded by
/// both an idle timeout and a closed snapshot budget, so relay churn cannot
/// keep a pre-write action alive forever.
pub fn set_following(engine: Arc<Engine>, target: PublicKey, change: FollowChange) -> FollowAction {
    set_following_with_timeout(engine, target, change, ACQUISITION_TIMEOUT)
}

fn set_following_with_timeout(
    engine: Arc<Engine>,
    target: PublicKey,
    change: FollowChange,
    timeout: Duration,
) -> FollowAction {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(FollowActionStatus::Acquiring);
        let author = match engine.active_account() {
            Ok(Some(author)) => author,
            Ok(None) => {
                let _ = tx.send(FollowActionStatus::Failed(FollowActionFailure::SignedOut));
                return;
            }
            Err(_) => {
                let _ = tx.send(FollowActionStatus::Failed(
                    FollowActionFailure::EngineClosed,
                ));
                return;
            }
        };

        let subscription = match engine.observe(nmp::LiveQuery(active_account_demand())) {
            Ok(subscription) => subscription,
            Err(_) => {
                let _ = tx.send(FollowActionStatus::Failed(
                    FollowActionFailure::EngineClosed,
                ));
                return;
            }
        };
        let mut accumulator = Accumulator::default();
        let mut last_availability = FollowAvailability::Acquiring;
        let mut remaining_snapshots = MAX_ACQUISITION_SNAPSHOTS;

        let base = loop {
            if remaining_snapshots == 0 {
                let _ = tx.send(FollowActionStatus::Failed(
                    FollowActionFailure::AcquisitionTimedOut,
                ));
                return;
            }
            remaining_snapshots -= 1;
            match subscription.recv_timeout(timeout) {
                Ok((deltas, evidence)) => {
                    accumulator.apply(deltas);
                    let active = match engine.active_account() {
                        Ok(active) => active,
                        Err(_) => {
                            let _ = tx.send(FollowActionStatus::Failed(
                                FollowActionFailure::EngineClosed,
                            ));
                            return;
                        }
                    };
                    if active != Some(author) {
                        let _ = tx.send(FollowActionStatus::Failed(
                            FollowActionFailure::AccountChanged,
                        ));
                        return;
                    }
                    last_availability = availability(active, &evidence);
                    if last_availability == FollowAvailability::SourceUnavailable {
                        let _ = tx.send(FollowActionStatus::Failed(
                            FollowActionFailure::SourceUnavailable,
                        ));
                        return;
                    }
                    if last_availability == FollowAvailability::Ready {
                        let Some(base) = accumulator.base_for(author).cloned() else {
                            let _ = tx.send(FollowActionStatus::Failed(
                                FollowActionFailure::NoContactList,
                            ));
                            return;
                        };
                        break base;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    let failure = match last_availability {
                        FollowAvailability::CachedOnly => FollowActionFailure::CachedOnly,
                        FollowAvailability::SourceUnavailable => {
                            FollowActionFailure::SourceUnavailable
                        }
                        _ => FollowActionFailure::AcquisitionTimedOut,
                    };
                    let _ = tx.send(FollowActionStatus::Failed(failure));
                    return;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    let _ = tx.send(FollowActionStatus::Failed(
                        FollowActionFailure::EngineClosed,
                    ));
                    return;
                }
            }
        };

        let composed = match compose_follow_change(author, &base, target, change, Timestamp::now())
        {
            Ok(value) => value,
            Err(error) => {
                let _ = tx.send(FollowActionStatus::Failed(FollowActionFailure::Compose(
                    error,
                )));
                return;
            }
        };
        let intent = match composed {
            ComposeFollowResult::NoChange => {
                let _ = tx.send(FollowActionStatus::NoChange {
                    following: change == FollowChange::Follow,
                });
                return;
            }
            ComposeFollowResult::Publish(intent) => *intent,
        };

        let receipt = match engine.publish_tracked(intent) {
            Ok(receipt) => receipt,
            Err(nmp::EngineError::EngineClosed) => {
                let _ = tx.send(FollowActionStatus::Failed(
                    FollowActionFailure::EngineClosed,
                ));
                return;
            }
            Err(_) => {
                let _ = tx.send(FollowActionStatus::Failed(
                    FollowActionFailure::ReceiptUnavailable,
                ));
                return;
            }
        };
        let receipt_id = receipt.id.0;
        while let Ok(status) = receipt.statuses.recv() {
            if tx
                .send(FollowActionStatus::Receipt { receipt_id, status })
                .is_err()
            {
                return;
            }
        }
    });
    FollowAction { statuses: rx }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nmp::{EngineConfig, RelayUrl, SourceEvidence};
    use nostr::Keys;

    #[test]
    fn signed_out_action_fails_typed_without_a_write() {
        let engine = Arc::new(Engine::new(EngineConfig::default()).unwrap());
        let action = set_following_with_timeout(
            engine,
            Keys::generate().public_key(),
            FollowChange::Follow,
            Duration::from_millis(10),
        );
        assert_eq!(action.recv().unwrap(), FollowActionStatus::Acquiring);
        assert_eq!(
            action.recv().unwrap(),
            FollowActionStatus::Failed(FollowActionFailure::SignedOut)
        );
    }

    #[test]
    fn logged_in_without_sources_times_out_instead_of_inventing_empty_contacts() {
        let engine = Arc::new(Engine::new(EngineConfig::default()).unwrap());
        let author = Keys::generate();
        engine
            .add_account(&author.secret_key().to_secret_hex())
            .unwrap();
        engine
            .set_active_account(Some(author.public_key()))
            .unwrap();
        let action = set_following_with_timeout(
            engine,
            Keys::generate().public_key(),
            FollowChange::Follow,
            Duration::from_millis(20),
        );
        assert_eq!(action.recv().unwrap(), FollowActionStatus::Acquiring);
        assert_eq!(
            action.recv().unwrap(),
            FollowActionStatus::Failed(FollowActionFailure::SourceUnavailable)
        );
    }

    #[test]
    fn reconciled_absence_is_visible_but_not_an_editable_empty_list() {
        let author = Keys::generate().public_key();
        let target = Keys::generate().public_key();
        let evidence = AcquisitionEvidence {
            sources: vec![SourceEvidence {
                relay: RelayUrl::parse("wss://relay.example").unwrap(),
                reconciled_through: Some(Timestamp::from_secs(10)),
                status: SourceStatus::Requesting,
            }],
            shortfall: vec![],
        };

        let snapshot = project(Some(author), target, &Accumulator::default(), &evidence);
        assert_eq!(snapshot.relationship, FollowRelationship::NotFollowing);
        assert_eq!(snapshot.availability, FollowAvailability::NoContactList);
        assert_eq!(snapshot.base_event_id, None);
    }
}
