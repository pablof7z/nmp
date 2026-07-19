use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::mpsc::{RecvError, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use nmp::{
    fifo_channel, AcquisitionEvidence, AsyncFifoReceiver, Engine, Event, EventId, FifoReceiver,
    FifoRecvError, FifoRecvTimeoutError, FifoSender, ObservationCancel, PublicKey, RowDelta,
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
    /// The requested target public key did not parse (hex). No worker started.
    InvalidTarget {
        got: String,
    },
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

/// #704: the latest-wins follow observer is an async task on the engine
/// runtime (no dedicated OS thread). It drains the waker-driven async row
/// mailbox and folds each frame into the latest slot; the blocking
/// `FollowObservation::recv`/`recv_timeout` consumer reads that slot. There is
/// no admission slot to reserve — nothing is refused.
pub fn observe_following(
    engine: Arc<Engine>,
    target: PublicKey,
) -> Result<FollowObservation, nmp::EngineError> {
    let runtime = engine.adapter_runtime()?;
    let subscription = engine.observe_async(nmp::LiveQuery(active_account_demand()), None)?;
    let cancel = subscription.cancel_handle();
    let latest = Arc::new(LatestSlot::default());
    let producer = latest.clone();
    runtime.spawn(async move {
        let mut accumulator = Accumulator::default();
        while let Ok(Some(frame)) = subscription.next().await {
            accumulator.apply(frame.deltas);
            let active = engine.active_account().ok().flatten();
            producer.send(project(active, target, &accumulator, &frame.evidence));
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
    subscription: nmp::AsyncSubscription,
    engine: Arc<Engine>,
    target: PublicKey,
    accumulator: Mutex<Accumulator>,
}

impl AsyncFollowObservation {
    /// Await the next relationship snapshot, or `None` once the underlying
    /// demand is withdrawn. [`nmp::ConcurrentNext`] on an overlapping call.
    pub async fn next(&self) -> Result<Option<FollowSnapshot>, nmp::ConcurrentNext> {
        match self.subscription.next().await? {
            Some(frame) => {
                let mut accumulator = self.accumulator.lock().unwrap();
                accumulator.apply(frame.deltas);
                let active = self.engine.active_account().ok().flatten();
                Ok(Some(project(
                    active,
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
) -> Result<AsyncFollowObservation, nmp::EngineError> {
    let subscription = engine.observe_async(nmp::LiveQuery(active_account_demand()), None)?;
    Ok(AsyncFollowObservation {
        subscription,
        engine,
        target,
        accumulator: Mutex::new(Accumulator::default()),
    })
}

pub struct FollowAction {
    statuses: FifoReceiver<FollowActionStatus>,
}

type FollowActionFuture = Box<
    dyn FnOnce(FifoSender<FollowActionStatus>) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send,
>;

/// A prepared follow action whose worker has not started yet. Native bridges
/// use this split to establish their observer before any acquisition or write
/// can run unseen.
///
/// #704: the worker is an async task on the engine runtime, not a reserved
/// blocking thread. The status FIFO's single [`FifoSender`] lives here until
/// [`Self::start`] runs: if the engine is already closed (no runtime handle)
/// the runner keeps the sender and emits the terminal failure through it; on
/// success the sender is handed to the async worker.
pub struct FollowActionRunner {
    task: FollowActionFuture,
    sender: FifoSender<FollowActionStatus>,
    runtime: Result<tokio::runtime::Handle, nmp::EngineError>,
}

impl FollowActionRunner {
    pub fn start(self) {
        let Self {
            task,
            sender,
            runtime,
        } = self;
        match runtime {
            Ok(runtime) => {
                runtime.spawn(task(sender));
            }
            Err(error) => {
                sender.send(FollowActionStatus::Failed(engine_failure(error)));
            }
        }
    }
}

impl FollowAction {
    pub fn recv(&self) -> Result<FollowActionStatus, FifoRecvError> {
        self.statuses.recv()
    }

    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<FollowActionStatus, FifoRecvTimeoutError> {
        self.statuses.recv_timeout(timeout)
    }

    /// The pull-based async surface over the same status FIFO (#680). The FFI
    /// follow-action stream awaits `next()` on this; direct-Rust drains keep
    /// using [`Self::recv`]/[`Self::recv_timeout`].
    #[must_use]
    pub fn into_async(self) -> AsyncFifoReceiver<FollowActionStatus> {
        self.statuses.into_async()
    }

    /// A one-shot action that never starts a worker: it carries exactly the one
    /// terminal `Failed(failure)` fact, then ends. Used for a pre-worker
    /// rejection (e.g. an unparseable target) that must still be observed as a
    /// terminal follow-action status rather than a separate error channel.
    #[must_use]
    pub fn one_shot_failure(failure: FollowActionFailure) -> Self {
        let (sender, statuses) = fifo_channel();
        sender.send(FollowActionStatus::Failed(failure));
        // `sender` drops here → the FIFO ends after the single fact drains.
        Self { statuses }
    }
}

/// Start NMP's simple NIP-02 action. The acquisition/readiness policy,
/// exact-base edit, atomic conflict guard, signer, durable outbox routing,
/// and receipt stream all remain in Rust. A UI merely observes these states
/// and asks for `Follow` or `Unfollow`. Initial acquisition is bounded by
/// both an idle timeout and a closed snapshot budget, so relay churn cannot
/// keep a pre-write action alive forever.
pub fn set_following(engine: Arc<Engine>, target: PublicKey, change: FollowChange) -> FollowAction {
    let (action, runner) = prepare_set_following(engine, target, change);
    runner.start();
    action
}

/// Prepare NMP's simple NIP-02 action without starting its worker. The caller
/// must start the returned runner after its observation path is established.
pub fn prepare_set_following(
    engine: Arc<Engine>,
    target: PublicKey,
    change: FollowChange,
) -> (FollowAction, FollowActionRunner) {
    prepare_set_following_with_timeout(engine, target, change, ACQUISITION_TIMEOUT)
}

#[cfg(test)]
fn set_following_with_timeout(
    engine: Arc<Engine>,
    target: PublicKey,
    change: FollowChange,
    timeout: Duration,
) -> FollowAction {
    let (action, runner) = prepare_set_following_with_timeout(engine, target, change, timeout);
    runner.start();
    action
}

fn prepare_set_following_with_timeout(
    engine: Arc<Engine>,
    target: PublicKey,
    change: FollowChange,
    timeout: Duration,
) -> (FollowAction, FollowActionRunner) {
    let (sender, statuses) = fifo_channel();
    let runtime = engine.adapter_runtime();
    // #704: the follow-action worker is an async task. Its acquisition wait is
    // `AsyncSubscription::next()` under a per-snapshot `tokio::time::timeout`,
    // and its receipt streaming awaits the async status FIFO — no OS thread is
    // held while the engine round-trips or the write settles.
    let task: FollowActionFuture = Box::new(move |tx: FifoSender<FollowActionStatus>| {
        Box::pin(async move {
            tx.send(FollowActionStatus::Acquiring);
            let author = match engine.active_account() {
                Ok(Some(author)) => author,
                Ok(None) => {
                    tx.send(FollowActionStatus::Failed(FollowActionFailure::SignedOut));
                    return;
                }
                Err(error) => {
                    tx.send(FollowActionStatus::Failed(engine_failure(error)));
                    return;
                }
            };

            let subscription =
                match engine.observe_async(nmp::LiveQuery(active_account_demand()), None) {
                    Ok(subscription) => subscription,
                    Err(error) => {
                        tx.send(FollowActionStatus::Failed(engine_failure(error)));
                        return;
                    }
                };
            let mut accumulator = Accumulator::default();
            let mut last_availability = FollowAvailability::Acquiring;
            let mut remaining_snapshots = MAX_ACQUISITION_SNAPSHOTS;

            let base = loop {
                if remaining_snapshots == 0 {
                    tx.send(FollowActionStatus::Failed(
                        FollowActionFailure::AcquisitionTimedOut,
                    ));
                    return;
                }
                remaining_snapshots -= 1;
                match tokio::time::timeout(timeout, subscription.next()).await {
                    Ok(Ok(Some(frame))) => {
                        accumulator.apply(frame.deltas);
                        let active = match engine.active_account() {
                            Ok(active) => active,
                            Err(error) => {
                                tx.send(FollowActionStatus::Failed(engine_failure(error)));
                                return;
                            }
                        };
                        if active != Some(author) {
                            tx.send(FollowActionStatus::Failed(
                                FollowActionFailure::AccountChanged,
                            ));
                            return;
                        }
                        last_availability = availability(active, &frame.evidence);
                        if last_availability == FollowAvailability::SourceUnavailable {
                            tx.send(FollowActionStatus::Failed(
                                FollowActionFailure::SourceUnavailable,
                            ));
                            return;
                        }
                        if last_availability == FollowAvailability::Ready {
                            let Some(base) = accumulator.base_for(author).cloned() else {
                                tx.send(FollowActionStatus::Failed(
                                    FollowActionFailure::NoContactList,
                                ));
                                return;
                            };
                            break base;
                        }
                    }
                    // Per-snapshot deadline elapsed.
                    Err(_elapsed) => {
                        let failure = match last_availability {
                            FollowAvailability::CachedOnly => FollowActionFailure::CachedOnly,
                            FollowAvailability::SourceUnavailable => {
                                FollowActionFailure::SourceUnavailable
                            }
                            _ => FollowActionFailure::AcquisitionTimedOut,
                        };
                        tx.send(FollowActionStatus::Failed(failure));
                        return;
                    }
                    // Demand withdrawn / engine closed (`Ok(None)`), or an
                    // overlapping `next()` (`Err`, unreachable for this single
                    // sequential consumer).
                    Ok(Ok(None)) | Ok(Err(_)) => {
                        tx.send(FollowActionStatus::Failed(
                            FollowActionFailure::EngineClosed,
                        ));
                        return;
                    }
                }
            };

            let composed =
                match compose_follow_change(author, &base, target, change, Timestamp::now()) {
                    Ok(value) => value,
                    Err(error) => {
                        tx.send(FollowActionStatus::Failed(FollowActionFailure::Compose(
                            error,
                        )));
                        return;
                    }
                };
            let intent = match composed {
                ComposeFollowResult::NoChange => {
                    tx.send(FollowActionStatus::NoChange {
                        following: change == FollowChange::Follow,
                    });
                    return;
                }
                ComposeFollowResult::Publish(intent) => *intent,
            };

            let receipt = match engine.publish_tracked(intent) {
                Ok(receipt) => receipt,
                Err(error) => {
                    let failure = match error {
                        nmp::EngineError::EngineClosed => FollowActionFailure::EngineClosed,
                        _ => FollowActionFailure::ReceiptUnavailable,
                    };
                    tx.send(FollowActionStatus::Failed(failure));
                    return;
                }
            };
            let receipt_id = receipt.id.0;
            let statuses = receipt.statuses.into_async();
            while let Ok(Some(status)) = statuses.next().await {
                tx.send(FollowActionStatus::Receipt { receipt_id, status });
            }
        })
    });
    (
        FollowAction { statuses },
        FollowActionRunner {
            task,
            sender,
            runtime,
        },
    )
}

fn engine_failure(_error: nmp::EngineError) -> FollowActionFailure {
    // #704: the only operational engine failure a follow-action can hit is a
    // closed engine (the reserve/`ThreadUnavailable` admission path is gone).
    FollowActionFailure::EngineClosed
}

#[cfg(test)]
mod tests {
    use super::*;
    use nmp::{AccessContext, EngineConfig, RelayUrl, SourceEvidence};
    use nostr::Keys;

    // #704: three tests were deleted here —
    // `injected_follow_observer_refusal_is_typed_and_cancels_the_subscription`,
    // `injected_follow_action_worker_refusal_is_the_only_terminal_status`, and
    // `acquisition_thread_refusal_is_not_collapsed_to_engine_closed`. All three
    // asserted the removed executor admission-refusal surface: the
    // `observe_following_with_spawn` / `FollowActionRunner::start_with` spawn
    // seams and the `FollowActionFailure::ThreadUnavailable` variant no longer
    // exist — the observer/action workers are async tasks on the engine runtime
    // that reserve nothing and cannot be refused.

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
                access: AccessContext::Public,
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
