use std::collections::BTreeMap;
use std::io;
use std::sync::mpsc::{self, Receiver, RecvError, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::Duration;

use nmp::{
    AcquisitionEvidence, Engine, Event, EventId, RelayUrl, RowDelta, ShortfallFact, SourceStatus,
    Timestamp, WriteStatus,
};

use crate::demand::active_account_demand;
use crate::edit::{
    compose_relay_change, ComposeRelayChangeError, ComposeRelayChangeResult, RelayChange,
};

const ACQUISITION_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_ACQUISITION_SNAPSHOTS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayActionFailure {
    SignedOut,
    AccountChanged,
    AcquisitionTimedOut,
    CachedOnly,
    SourceUnavailable,
    Compose(ComposeRelayChangeError),
    EngineClosed,
    ReceiptUnavailable,
    ThreadUnavailable { component: String, reason: String },
    ExecutorSaturated { component: String, capacity: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayActionStatus {
    Acquiring,
    NoChange {
        present: bool,
    },
    Receipt {
        receipt_id: u64,
        status: WriteStatus,
    },
    Failed(RelayActionFailure),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditAvailability {
    Acquiring,
    Ready,
    CachedOnly,
    SourceUnavailable,
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

    fn base_for(&self, active: nostr::PublicKey) -> Option<&Event> {
        self.rows
            .values()
            .find(|event| event.pubkey == active && event.kind == nostr::Kind::Custom(10009))
    }
}

fn availability(evidence: &AcquisitionEvidence) -> EditAvailability {
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
        return EditAvailability::SourceUnavailable;
    }
    if evidence.sources.is_empty()
        || evidence
            .sources
            .iter()
            .any(|source| source.reconciled_through.is_none())
    {
        return EditAvailability::Acquiring;
    }
    if evidence
        .sources
        .iter()
        .any(|source| source.status == SourceStatus::Disconnected)
    {
        return EditAvailability::CachedOnly;
    }
    if evidence.sources.iter().all(|source| {
        source.status == SourceStatus::Requesting && source.reconciled_through.is_some()
    }) && evidence.shortfall.is_empty()
    {
        EditAvailability::Ready
    } else {
        EditAvailability::Acquiring
    }
}

pub struct RelayAction {
    statuses: Receiver<RelayActionStatus>,
}

impl RelayAction {
    pub fn recv(&self) -> Result<RelayActionStatus, RecvError> {
        self.statuses.recv()
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<RelayActionStatus, RecvTimeoutError> {
        self.statuses.recv_timeout(timeout)
    }
}

/// Prepared separately so FFI can establish its status observer before the
/// acquisition worker emits any state.
pub struct RelayActionRunner {
    task: Box<dyn FnOnce() + Send + 'static>,
    failures: Sender<RelayActionStatus>,
    reservation: Result<nmp::NativeTaskReservation, nmp::EngineError>,
    cancellation: Result<nmp::NativeTaskCancel, nmp::EngineError>,
}

impl RelayActionRunner {
    pub fn start(self) {
        let Self {
            task,
            failures,
            reservation,
            cancellation,
        } = self;
        let reservation = match reservation {
            Ok(value) => value,
            Err(error) => {
                let _ = failures.send(RelayActionStatus::Failed(engine_failure(error)));
                return;
            }
        };
        let cancellation = match cancellation {
            Ok(value) => value,
            Err(error) => {
                let _ = failures.send(RelayActionStatus::Failed(engine_failure(error)));
                return;
            }
        };
        if let Err(error) = reservation
            .spawn_with_cancel(move || cancellation.cancel(), task)
            .map_err(|error| io::Error::other(error.to_string()))
        {
            let _ = failures.send(RelayActionStatus::Failed(
                RelayActionFailure::ThreadUnavailable {
                    component: "NIP-51 relay-list action".to_string(),
                    reason: error.to_string(),
                },
            ));
        }
    }
}

pub fn set_relay(engine: Arc<Engine>, relay: RelayUrl, change: RelayChange) -> RelayAction {
    let (action, runner) = prepare_set_relay(engine, relay, change);
    runner.start();
    action
}

pub fn prepare_set_relay(
    engine: Arc<Engine>,
    relay: RelayUrl,
    change: RelayChange,
) -> (RelayAction, RelayActionRunner) {
    prepare_set_relay_with_timeout(engine, relay, change, ACQUISITION_TIMEOUT)
}

fn prepare_set_relay_with_timeout(
    engine: Arc<Engine>,
    relay: RelayUrl,
    change: RelayChange,
    timeout: Duration,
) -> (RelayAction, RelayActionRunner) {
    let (tx, rx) = mpsc::channel();
    let failures = tx.clone();
    let reservation = engine.reserve_native_task("NIP-51 relay-list action");
    let cancellation = engine.native_task_cancel();
    let task = Box::new(move || {
        let _ = tx.send(RelayActionStatus::Acquiring);
        let author = match engine.active_account() {
            Ok(Some(author)) => author,
            Ok(None) => {
                let _ = tx.send(RelayActionStatus::Failed(RelayActionFailure::SignedOut));
                return;
            }
            Err(error) => {
                let _ = tx.send(RelayActionStatus::Failed(engine_failure(error)));
                return;
            }
        };
        let subscription = match engine.observe(nmp::LiveQuery(active_account_demand())) {
            Ok(subscription) => subscription,
            Err(error) => {
                let _ = tx.send(RelayActionStatus::Failed(engine_failure(error)));
                return;
            }
        };
        let mut accumulator = Accumulator::default();
        let mut last_availability = EditAvailability::Acquiring;
        let mut remaining_snapshots = MAX_ACQUISITION_SNAPSHOTS;

        let base = loop {
            if remaining_snapshots == 0 {
                let _ = tx.send(RelayActionStatus::Failed(
                    RelayActionFailure::AcquisitionTimedOut,
                ));
                return;
            }
            remaining_snapshots -= 1;
            match subscription.recv_timeout(timeout) {
                Ok((deltas, evidence)) => {
                    accumulator.apply(deltas);
                    let active = match engine.active_account() {
                        Ok(value) => value,
                        Err(error) => {
                            let _ = tx.send(RelayActionStatus::Failed(engine_failure(error)));
                            return;
                        }
                    };
                    if active != Some(author) {
                        let _ = tx.send(RelayActionStatus::Failed(
                            RelayActionFailure::AccountChanged,
                        ));
                        return;
                    }
                    last_availability = availability(&evidence);
                    if last_availability == EditAvailability::SourceUnavailable {
                        let _ = tx.send(RelayActionStatus::Failed(
                            RelayActionFailure::SourceUnavailable,
                        ));
                        return;
                    }
                    if last_availability == EditAvailability::Ready {
                        break accumulator.base_for(author).cloned();
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    let failure = match last_availability {
                        EditAvailability::CachedOnly => RelayActionFailure::CachedOnly,
                        EditAvailability::SourceUnavailable => {
                            RelayActionFailure::SourceUnavailable
                        }
                        _ => RelayActionFailure::AcquisitionTimedOut,
                    };
                    let _ = tx.send(RelayActionStatus::Failed(failure));
                    return;
                }
                Err(RecvTimeoutError::Disconnected) => {
                    let _ = tx.send(RelayActionStatus::Failed(RelayActionFailure::EngineClosed));
                    return;
                }
            }
        };

        let composed =
            match compose_relay_change(author, base.as_ref(), &relay, change, Timestamp::now()) {
                Ok(value) => value,
                Err(error) => {
                    let _ = tx.send(RelayActionStatus::Failed(RelayActionFailure::Compose(
                        error,
                    )));
                    return;
                }
            };
        let intent = match composed {
            ComposeRelayChangeResult::NoChange => {
                let _ = tx.send(RelayActionStatus::NoChange {
                    present: change == RelayChange::Add,
                });
                return;
            }
            ComposeRelayChangeResult::Publish(intent) => *intent,
        };
        let receipt = match engine.publish_tracked(intent) {
            Ok(receipt) => receipt,
            Err(error) => {
                let failure = match error {
                    nmp::EngineError::ThreadUnavailable { .. }
                    | nmp::EngineError::ExecutorSaturated { .. } => engine_failure(error),
                    nmp::EngineError::EngineClosed => RelayActionFailure::EngineClosed,
                    _ => RelayActionFailure::ReceiptUnavailable,
                };
                let _ = tx.send(RelayActionStatus::Failed(failure));
                return;
            }
        };
        let receipt_id = receipt.id.0;
        while let Ok(status) = receipt.statuses.recv() {
            if tx
                .send(RelayActionStatus::Receipt { receipt_id, status })
                .is_err()
            {
                return;
            }
        }
    });
    (
        RelayAction { statuses: rx },
        RelayActionRunner {
            task,
            failures,
            reservation,
            cancellation,
        },
    )
}

fn engine_failure(error: nmp::EngineError) -> RelayActionFailure {
    match error {
        nmp::EngineError::ThreadUnavailable { component, reason } => {
            RelayActionFailure::ThreadUnavailable { component, reason }
        }
        nmp::EngineError::ExecutorSaturated {
            component,
            capacity,
        } => RelayActionFailure::ExecutorSaturated {
            component,
            capacity,
        },
        _ => RelayActionFailure::EngineClosed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nmp::EngineConfig;
    use nostr::Keys;

    #[test]
    fn signed_out_action_fails_without_a_write() {
        let engine = Arc::new(Engine::new(EngineConfig::default()).unwrap());
        let (action, runner) = prepare_set_relay_with_timeout(
            engine,
            RelayUrl::parse("wss://relay.example").unwrap(),
            RelayChange::Add,
            Duration::from_millis(10),
        );
        runner.start();
        assert_eq!(action.recv().unwrap(), RelayActionStatus::Acquiring);
        assert_eq!(
            action.recv().unwrap(),
            RelayActionStatus::Failed(RelayActionFailure::SignedOut)
        );
    }

    #[test]
    fn account_without_sources_never_invents_an_empty_list() {
        let engine = Arc::new(Engine::new(EngineConfig::default()).unwrap());
        let author = Keys::generate();
        engine
            .add_account(&author.secret_key().to_secret_hex())
            .unwrap();
        engine
            .set_active_account(Some(author.public_key()))
            .unwrap();
        let (action, runner) = prepare_set_relay_with_timeout(
            engine,
            RelayUrl::parse("wss://relay.example").unwrap(),
            RelayChange::Add,
            Duration::from_millis(20),
        );
        runner.start();
        assert_eq!(action.recv().unwrap(), RelayActionStatus::Acquiring);
        assert_eq!(
            action.recv().unwrap(),
            RelayActionStatus::Failed(RelayActionFailure::SourceUnavailable)
        );
    }
}
