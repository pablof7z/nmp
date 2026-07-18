use std::collections::BTreeMap;
use std::io;
use std::sync::mpsc::{RecvError, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use nmp::{
    fifo_channel, AcquisitionEvidence, AsyncFifoReceiver, Engine, Event, EventId, FifoReceiver,
    FifoSender, ObservationCancel, PublicKey, RowDelta, ShortfallFact, SourceStatus, Timestamp,
    WriteStatus,
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
    ThreadUnavailable {
        component: String,
        reason: String,
    },
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
    let task_cancel = engine.native_task_cancel()?;
    observe_following_with_spawn(engine, target, move |reservation, task| {
        reservation
            .spawn_with_cancel(move || task_cancel.cancel(), task)
            .map_err(|error| io::Error::other(error.to_string()))
    })
}

fn observe_following_with_spawn(
    engine: Arc<Engine>,
    target: PublicKey,
    spawn: impl FnOnce(nmp::NativeTaskReservation, Box<dyn FnOnce() + Send + 'static>) -> io::Result<()>,
) -> Result<FollowObservation, nmp::EngineError> {
    let reservation = engine.reserve_native_task("NIP-02 follow observer")?;
    let subscription = engine.observe(nmp::LiveQuery(active_account_demand()), None)?;
    let cancel = subscription.cancel_handle();
    let latest = Arc::new(LatestSlot::default());
    let producer = latest.clone();

    if let Err(error) = spawn(
        reservation,
        Box::new(move || {
            let mut accumulator = Accumulator::default();
            while let Ok(frame) = subscription.recv() {
                accumulator.apply(frame.deltas);
                let active = engine.active_account().ok().flatten();
                producer.send(project(active, target, &accumulator, &frame.evidence));
            }
            producer.close();
        }),
    ) {
        cancel.cancel();
        return Err(nmp::EngineError::ThreadUnavailable {
            component: "NIP-02 follow observer".to_string(),
            reason: error.to_string(),
        });
    }

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

/// A prepared follow action whose worker has not started yet. Native bridges
/// use this split to establish their observer before any acquisition or write
/// can run unseen.
///
/// The status FIFO's single [`FifoSender`] lives here (not in the worker
/// closure) until [`Self::start`] runs: on a reservation/spawn refusal the
/// runner keeps the sender and emits the terminal failure through it; on
/// success the sender is handed to the worker. This is what lets the
/// single-producer channel report a pre-start failure without a second sender.
pub struct FollowActionRunner {
    task: Box<dyn FnOnce(FifoSender<FollowActionStatus>) + Send + 'static>,
    sender: FifoSender<FollowActionStatus>,
    reservation: Result<nmp::NativeTaskReservation, nmp::EngineError>,
    cancellation: Result<nmp::NativeTaskCancel, nmp::EngineError>,
}

impl FollowActionRunner {
    pub fn start(self) {
        self.start_with(|reservation, cancellation| {
            reservation
                .start_with_cancel(move || cancellation.cancel())
                .map_err(|error| io::Error::other(error.to_string()))
        });
    }

    fn start_with(
        self,
        start: impl FnOnce(
            nmp::NativeTaskReservation,
            nmp::NativeTaskCancel,
        ) -> io::Result<nmp::StartedNativeTask>,
    ) {
        let Self {
            task,
            sender,
            reservation,
            cancellation,
        } = self;
        let reservation = match reservation {
            Ok(reservation) => reservation,
            Err(error) => {
                sender.send(FollowActionStatus::Failed(engine_failure(error)));
                return;
            }
        };
        let cancellation = match cancellation {
            Ok(cancellation) => cancellation,
            Err(error) => {
                sender.send(FollowActionStatus::Failed(engine_failure(error)));
                return;
            }
        };
        // Start the OS thread BEFORE the sender crosses into it: a spawn
        // refusal leaves `sender` here to carry the terminal failure, while a
        // success hands it to the worker via the one-shot starter.
        match start(reservation, cancellation) {
            Ok(starter) => starter.run(move || task(sender)),
            Err(error) => {
                sender.send(FollowActionStatus::Failed(
                    FollowActionFailure::ThreadUnavailable {
                        component: "NIP-02 follow action".to_string(),
                        reason: error.to_string(),
                    },
                ));
            }
        }
    }
}

impl FollowAction {
    pub fn recv(&self) -> Result<FollowActionStatus, RecvError> {
        self.statuses.recv()
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<FollowActionStatus, RecvTimeoutError> {
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
    let reservation = engine.reserve_native_task("NIP-02 follow action");
    let cancellation = engine.native_task_cancel();
    let task = Box::new(move |tx: FifoSender<FollowActionStatus>| {
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

        let subscription = match engine.observe(nmp::LiveQuery(active_account_demand()), None) {
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
            match subscription.recv_timeout(timeout) {
                Ok(frame) => {
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
                Err(RecvTimeoutError::Timeout) => {
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
                Err(RecvTimeoutError::Disconnected) => {
                    tx.send(FollowActionStatus::Failed(
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
                    nmp::EngineError::ThreadUnavailable { .. } => engine_failure(error),
                    nmp::EngineError::EngineClosed => FollowActionFailure::EngineClosed,
                    _ => FollowActionFailure::ReceiptUnavailable,
                };
                tx.send(FollowActionStatus::Failed(failure));
                return;
            }
        };
        let receipt_id = receipt.id.0;
        while let Ok(status) = receipt.statuses.recv() {
            tx.send(FollowActionStatus::Receipt { receipt_id, status });
        }
    });
    (
        FollowAction { statuses },
        FollowActionRunner {
            task,
            sender,
            reservation,
            cancellation,
        },
    )
}

fn engine_failure(error: nmp::EngineError) -> FollowActionFailure {
    match error {
        nmp::EngineError::ThreadUnavailable { component, reason } => {
            FollowActionFailure::ThreadUnavailable { component, reason }
        }
        _ => FollowActionFailure::EngineClosed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nmp::{AccessContext, EngineConfig, RelayUrl, SourceEvidence};
    use nostr::Keys;

    #[test]
    fn injected_follow_observer_refusal_is_typed_and_cancels_the_subscription() {
        let engine = Arc::new(Engine::new(EngineConfig::default()).unwrap());
        let result =
            observe_following_with_spawn(engine, Keys::generate().public_key(), |_, task| {
                drop(task);
                Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "injected NIP-02 observer pressure",
                ))
            });
        assert!(matches!(
            result,
            Err(nmp::EngineError::ThreadUnavailable { component, reason })
                if component == "NIP-02 follow observer"
                    && reason == "injected NIP-02 observer pressure"
        ));
    }

    #[test]
    fn injected_follow_action_worker_refusal_is_the_only_terminal_status() {
        let engine = Arc::new(Engine::new(EngineConfig::default()).unwrap());
        let (action, runner) = prepare_set_following_with_timeout(
            engine,
            Keys::generate().public_key(),
            FollowChange::Follow,
            Duration::from_millis(10),
        );
        runner.start_with(|reservation, cancellation| {
            drop(reservation);
            drop(cancellation);
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "injected NIP-02 action pressure",
            ))
        });
        assert_eq!(
            action.recv().unwrap(),
            FollowActionStatus::Failed(FollowActionFailure::ThreadUnavailable {
                component: "NIP-02 follow action".to_string(),
                reason: "injected NIP-02 action pressure".to_string(),
            })
        );
        assert_eq!(action.recv(), Err(RecvError));
    }

    #[test]
    fn acquisition_thread_refusal_is_not_collapsed_to_engine_closed() {
        assert_eq!(
            engine_failure(nmp::EngineError::ThreadUnavailable {
                component: "engine command bridge".to_string(),
                reason: "injected pressure".to_string(),
            }),
            FollowActionFailure::ThreadUnavailable {
                component: "engine command bridge".to_string(),
                reason: "injected pressure".to_string(),
            }
        );
    }

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
