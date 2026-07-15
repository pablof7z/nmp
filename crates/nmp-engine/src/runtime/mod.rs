//! The async edge (plan §2 position 2). `EngineThread` spawns TWO dedicated
//! OS threads:
//!
//! - the **engine thread**, which owns `core::EngineCore` and runs a
//!   deadline-armed blocking recv loop (D8: the existing blocking
//!   `mpsc::Receiver<Cmd>::recv()` grows a timeout, never a poll-loop timer
//!   thread — see `engine_loop`'s doc and
//!   `docs/design/retraction-and-negative-deltas.md` §3.3, #39): with no
//!   deadline pending it blocks on plain `recv()`; with one pending it
//!   `recv_timeout`s exactly until `core::EngineCore::next_deadline()`, and a
//!   timeout dispatches `EngineMsg::Tick` (NIP-40 expiry + the neg-liveness
//!   sweep) before re-arming from the freshly-recomputed deadline — for
//!   every command it calls `EngineCore::handle`/`::tick` and dispatches the
//!   returned `core::Effect`s to `nmp_transport::Pool::send`, the
//!   `nmp_signer` capability, and the app-facing channels;
//! - the **pool-bridge thread**, a tiny translator that blocking-`recv`s
//!   `nmp_transport::PoolEvent`s (the pool's OWN `mio` worker threads push
//!   these) and forwards each as a `core::EngineMsg` onto the engine
//!   thread's inbox.
//!
//! `Handle` is the cheap, `Clone + Send` value the app holds: it sends
//! command `EngineMsg`s in (wrapped in the runtime-private [`Cmd`] envelope)
//! and gets back plain channels. The threading is entirely interior — the
//! app never sees `mio`, never sees a `PoolEvent`, never adopts a runtime
//! (§2, P1). `EngineCore` itself is `!Send`-friendly (M1's resolver keeps an
//! `Rc<RefCell<>>`) — it is constructed INSIDE the engine thread's closure
//! and never crosses a thread boundary; only `Send + 'static` VALUES (the
//! store, the directory, the signer) are moved into that closure at spawn
//! time.
//!
//! ## Two delivery channels, deliberately asymmetric (see the module's
//! `dispatch_effect`)
//!
//! `EngineCore` hands rows to a subscriber TWO ways: synchronously via the
//! `core::RowSink` passed to `EngineMsg::Subscribe`, and again via the
//! returned `Effect::EmitRows`. The two are NOT equivalent: `RowSink::
//! on_rows` carries only `Vec<RowDelta>` (no evidence), while `Effect::
//! EmitRows` carries `(HandleId, Vec<RowDelta>, AcquisitionEvidence)` — the
//! per-query acquisition evidence the read contract makes part of every
//! batch (`docs/design/scoped-evidence-49-12-plan.md`). This runtime
//! therefore picks ONE channel per plan's guidance: rows+evidence are
//! delivered from `Effect::EmitRows` alone (via
//! a `HandleId -> Sender` registry owned by the engine thread); the
//! `RowSink` registered at `Subscribe` time is a deliberate no-op so nothing
//! is delivered twice. Receipts have no such asymmetry — `ReceiptSink::
//! on_status` and `Effect::EmitReceipt` carry the exact same `WriteStatus`,
//! so the sink alone is the delivery channel and `Effect::EmitReceipt` is
//! acknowledged but not re-delivered.
//!
//! ## Reconnect-preamble bookkeeping
//!
//! `nmp_transport::Pool::set_reconnect_preamble` replaces the ENTIRE preamble
//! for a relay worker on every call ("last call wins" — see that method's
//! doc). `EngineCore`'s `Effect::Wire`/`Effect::Replay` are deltas/snapshots
//! of the CURRENT demand, not the preamble text itself, so this module keeps
//! its own per-SESSION `SubId -> wire REQ text` map (`Preambles`) and
//! re-derives the full preamble string list on every touch — see
//! `apply_wire_delta`/`apply_replay`. PROTECTED (`AccessContext::Nip42`)
//! sessions are the exception (#8): they never store a preamble at all — a
//! reconnected protected socket is unauthenticated until its own AUTH
//! completes, so nothing may auto-replay on it.

mod diagnostics_channel;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, RecvError, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel as cb;
use nmp_grammar::ConcreteFilter;
use nmp_resolver::{HandleId, LiveQuery};
use nmp_router::{RelayDirectory, SubId, WireDelta, WireOp, WireReq};
use nmp_signer::{
    pending_signer_cancellation, PendingSignerCancel, PendingSignerCancelled, PendingSignerOp,
    SignerOp, SigningCapability,
};
use nmp_store::EventStore;
use nostr::{
    ClientMessage, Event as SignedEvent, EventId, JsonUtil, PublicKey, RelayUrl, SubscriptionId,
    Timestamp, UnsignedEvent,
};

use nmp_transport::{
    DurableSendOutcome, HandoffResult, Pool, PoolConfig, PoolEvent, RelaySessionKey, WireFrame,
};

use crate::core::{
    self, AcquisitionEvidence, DiagnosticsSnapshot, Effect, EngineCore, EngineMsg,
    HistoryAdvanceError, HistoryBatch, HistoryQuery, HistorySessionId, HistorySink, PublishError,
    ReattachOutcome, ReceiptId, RelayAdmissionPolicy, Row, RowDelta, RowSink,
};
use crate::outbox::{ReceiptSink, WriteStatus};
use crate::relay_information::{
    RelayInformationCachePolicy, RelayInformationError, RelayInformationService,
    RelayInformationSnapshot,
};
use nmp_grammar::WriteIntent;

pub use diagnostics_channel::LatestReceiver;
use diagnostics_channel::{latest_channel, LatestSender};

/// NIP-11 may refine a capability decision, but a slow/unavailable HTTP
/// endpoint must not hold the WebSocket protocol path hostage. This is a
/// one-shot grace window, not polling; the eventual document still updates
/// diagnostics/cache after the behavioral probe has begun.
const NIP11_DECISION_GRACE: Duration = Duration::from_millis(250);

#[derive(Clone)]
struct EnginePoolSink {
    events: cb::Sender<PoolEvent>,
    stopping: cb::Receiver<()>,
}

struct EnginePoolRuntime {
    pool: Pool,
    stop: cb::Sender<()>,
    native_tasks: nmp_executor::Executor,
    relay_information: RelayInformationService,
}

impl nmp_transport::PoolEventSink for EnginePoolSink {
    fn on_event(&self, event: PoolEvent) {
        cb::select_biased! {
            recv(self.stopping) -> _ => {}
            send(self.events, event) -> _ => {}
        }
    }
}

/// One delivered batch for a live subscription: raw rows + the query's
/// per-source acquisition evidence (see the module doc's "two delivery
/// channels" note).
pub type RowsMsg = (Vec<RowDelta>, AcquisitionEvidence);
pub type HistoryMsg = HistoryBatch;

/// Receiver for one bounded, latest-wins history stream.
///
/// The single-slot mailbox stores a complete current frame. On receipt we
/// derive `deltas` against this receiver's last delivered frame, rather than
/// trusting the reducer-adjacent delta that may span an overwritten frame.
/// Both retained maps are bounded by the session's declared `max_rows`.
/// Like `std::sync::mpsc::Receiver`, this is a single-consumer value: it is
/// `Send` but deliberately not `Sync`.
///
/// ```compile_fail
/// use nmp_engine::runtime::HistoryReceiver;
/// fn require_sync<T: Sync>() {}
/// require_sync::<HistoryReceiver>();
/// ```
pub struct HistoryReceiver {
    batches: LatestReceiver<HistoryBatch>,
    delivered: RefCell<BTreeMap<EventId, Row>>,
}

impl HistoryReceiver {
    fn new(batches: LatestReceiver<HistoryBatch>) -> Self {
        Self {
            batches,
            delivered: RefCell::new(BTreeMap::new()),
        }
    }

    pub fn recv(&self) -> Result<HistoryBatch, RecvError> {
        let batch = self.batches.recv().ok_or(RecvError)?;
        let mut delivered = self.delivered.borrow_mut();
        Ok(Self::reconcile(&mut delivered, batch))
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<HistoryBatch, RecvTimeoutError> {
        let batch = self.batches.recv_timeout(timeout)?;
        let mut delivered = self.delivered.borrow_mut();
        Ok(Self::reconcile(&mut delivered, batch))
    }

    fn reconcile(delivered: &mut BTreeMap<EventId, Row>, mut batch: HistoryBatch) -> HistoryBatch {
        let current: BTreeMap<_, _> = batch
            .rows
            .iter()
            .cloned()
            .map(|row| (row.event.id, row))
            .collect();
        debug_assert_eq!(current.len(), batch.rows.len());

        let mut deltas = Vec::new();
        for row in &batch.rows {
            match delivered.get(&row.event.id) {
                None => deltas.push(RowDelta::Added(row.clone())),
                Some(previous) if previous.sources != row.sources => {
                    deltas.push(RowDelta::SourcesGrew {
                        id: row.event.id,
                        sources: row.sources.clone(),
                    });
                }
                Some(_) => {}
            }
        }
        for event_id in delivered.keys() {
            if !current.contains_key(event_id) {
                deltas.push(RowDelta::Removed(*event_id));
            }
        }
        *delivered = current;
        batch.deltas = deltas;
        batch
    }
}

#[cfg(test)]
mod history_mailbox_tests {
    use std::collections::BTreeSet;
    use std::thread;
    use std::time::{Duration, Instant};

    use nmp_grammar::{Binding, Filter};
    use nmp_router::FixtureDirectory;
    use nmp_store::{EventStore, MemoryStore, RelayObserved};
    use nostr::{Keys, Kind, UnsignedEvent};

    use super::*;
    use crate::core::{ShortfallFact, WindowLoad};

    fn row(keys: &Keys, created_at: u64, content: &str) -> Row {
        Row {
            event: UnsignedEvent::new(
                keys.public_key(),
                Timestamp::from(created_at),
                Kind::TextNote,
                Vec::new(),
                content,
            )
            .sign_with_keys(keys)
            .unwrap(),
            sources: BTreeSet::new(),
        }
    }

    fn canonical(mut rows: Vec<Row>) -> Vec<Row> {
        rows.sort_by(|a, b| {
            b.event
                .created_at
                .cmp(&a.event.created_at)
                .then_with(|| a.event.id.cmp(&b.event.id))
        });
        rows
    }

    fn batch(rows: Vec<Row>) -> HistoryBatch {
        HistoryBatch {
            rows,
            deltas: Vec::new(),
            evidence: AcquisitionEvidence::default(),
            load: WindowLoad::Idle,
        }
    }

    fn apply(rows: &mut BTreeMap<EventId, Row>, deltas: &[RowDelta]) {
        for delta in deltas {
            match delta {
                RowDelta::Added(row) => {
                    rows.insert(row.event.id, row.clone());
                }
                RowDelta::SourcesGrew { id, sources } => {
                    rows.get_mut(id).unwrap().sources = sources.clone();
                }
                RowDelta::Removed(id) => {
                    rows.remove(id);
                }
            }
        }
    }

    #[test]
    fn non_consuming_history_receiver_gets_one_latest_exact_bounded_state() {
        const MAX_ROWS: usize = 5;
        let keys = Keys::generate();
        let candidates: Vec<_> = (0..7)
            .map(|index| row(&keys, 100 + index, &format!("row-{index}")))
            .collect();
        let (tx, rx) = latest_channel();
        let rx = HistoryReceiver::new(rx);

        let first = canonical(vec![candidates[0].clone(), candidates[1].clone()]);
        tx.send(batch(first.clone()));
        let first_batch = rx.recv().unwrap();
        assert_eq!(first_batch.rows, first);
        let mut delivered = BTreeMap::new();
        apply(&mut delivered, &first_batch.deltas);

        let mut expected = Vec::new();
        for update in 0..10_000 {
            let omitted = update % candidates.len();
            expected = canonical(
                candidates
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| *index != omitted)
                    .take(MAX_ROWS)
                    .map(|(_, row)| row.clone())
                    .collect(),
            );
            tx.send(batch(expected.clone()));
        }

        let latest = rx.recv().unwrap();
        assert_eq!(latest.rows, expected);
        assert!(latest.rows.len() <= MAX_ROWS);
        apply(&mut delivered, &latest.deltas);
        assert_eq!(
            delivered,
            expected
                .iter()
                .cloned()
                .map(|row| (row.event.id, row))
                .collect()
        );
        assert_eq!(rx.delivered.borrow().len(), expected.len());
        assert!(
            matches!(
                rx.recv_timeout(Duration::from_millis(1)),
                Err(RecvTimeoutError::Timeout)
            ),
            "the 9,999 overwritten frames must not remain queued"
        );
    }

    #[test]
    fn conflation_keeps_authoritative_rows_and_latest_metadata_with_exact_rebased_deltas() {
        fn assert_send<T: Send>() {}
        assert_send::<HistoryReceiver>();

        let keys = Keys::generate();
        let removed = row(&keys, 101, "removed");
        let mut provenance_grew = row(&keys, 100, "provenance");
        let added = row(&keys, 99, "added");
        let overwritten = row(&keys, 98, "overwritten");
        let relay = RelayUrl::parse("wss://history-latest.example").unwrap();
        let (tx, rx) = latest_channel();
        let rx = HistoryReceiver::new(rx);

        let initial_rows = canonical(vec![removed.clone(), provenance_grew.clone()]);
        tx.send(HistoryBatch {
            rows: initial_rows,
            deltas: Vec::new(),
            evidence: AcquisitionEvidence::default(),
            load: WindowLoad::Idle,
        });
        let initial = rx.recv().unwrap();
        let mut delivered = BTreeMap::new();
        apply(&mut delivered, &initial.deltas);

        tx.send(HistoryBatch {
            rows: canonical(vec![provenance_grew.clone(), overwritten]),
            deltas: Vec::new(),
            evidence: AcquisitionEvidence::default(),
            load: WindowLoad::Requesting,
        });

        provenance_grew.sources.insert(relay);
        let latest_rows = canonical(vec![provenance_grew.clone(), added.clone()]);
        let latest_evidence = AcquisitionEvidence {
            sources: Vec::new(),
            shortfall: vec![ShortfallFact::NoResolvedDemand],
        };
        tx.send(HistoryBatch {
            rows: latest_rows.clone(),
            deltas: Vec::new(),
            evidence: latest_evidence.clone(),
            load: WindowLoad::Returned { added: 1 },
        });

        let latest = rx.recv().unwrap();
        assert_eq!(latest.rows, latest_rows);
        assert_eq!(latest.evidence, latest_evidence);
        assert_eq!(latest.load, WindowLoad::Returned { added: 1 });
        assert!(latest
            .deltas
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == added.event.id)));
        assert!(latest.deltas.iter().any(|delta| matches!(
            delta,
            RowDelta::SourcesGrew { id, sources }
                if *id == provenance_grew.event.id && *sources == provenance_grew.sources
        )));
        assert!(latest
            .deltas
            .iter()
            .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == removed.event.id)));
        assert_eq!(latest.deltas.len(), 3);
        apply(&mut delivered, &latest.deltas);
        assert_eq!(
            delivered,
            latest_rows
                .into_iter()
                .map(|row| (row.event.id, row))
                .collect()
        );
        assert!(matches!(
            rx.recv_timeout(Duration::from_millis(1)),
            Err(RecvTimeoutError::Timeout)
        ));
    }

    #[test]
    fn closing_history_mailbox_wakes_blocked_receiver() {
        let (tx, rx) = latest_channel();
        let rx = HistoryReceiver::new(rx);
        let waiter = thread::spawn(move || rx.recv());
        thread::sleep(Duration::from_millis(20));
        drop(tx);
        assert!(waiter.join().unwrap().is_err());
    }

    #[test]
    fn runtime_reply_drop_rolls_back_and_idle_cancel_and_shutdown_wake_receivers() {
        let _serial = RUNTIME_LIFECYCLE_TEST_LOCK.lock().unwrap();
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://history-runtime.example").unwrap();
        let events: Vec<_> = (0..3)
            .map(|index| row(&keys, 100 + index, &format!("runtime-{index}")))
            .map(|row| row.event)
            .collect();
        let mut store = MemoryStore::new();
        for event in &events {
            store
                .insert(
                    event.clone(),
                    RelayObserved::new(relay.clone(), Timestamp::from(500)),
                )
                .unwrap();
        }
        let query = HistoryQuery::new(
            LiveQuery::from_filter(Filter {
                authors: Some(Binding::Literal(BTreeSet::from([keys
                    .public_key()
                    .to_hex()]))),
                kinds: Some(BTreeSet::from([1])),
                ..Filter::default()
            }),
            1,
            3,
        );
        let (engine_thread, handle) = EngineThread::spawn(
            store,
            FixtureDirectory::new(),
            4,
            PoolConfig::default(),
            RelayAdmissionPolicy::default(),
        )
        .unwrap();

        let (history_handle, receiver) = handle.subscribe_history(query.clone()).unwrap();
        receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        // A request whose reply receiver is already dropped stages, fails to
        // reply, and rolls back — leaving the window exactly as before.
        let (reply, dropped_reply) = mpsc::channel();
        drop(dropped_reply);
        handle
            .inbox
            .send(Cmd::RequestRows {
                id: history_handle.0,
                at_least: 2,
                reply,
            })
            .unwrap();
        handle
            .request_rows(history_handle, 2)
            .expect("engine thread alive")
            .expect("the same request must retry after reply-drop rollback");
        let deadline = Instant::now() + Duration::from_secs(1);
        let loaded = loop {
            let batch = receiver
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap();
            if matches!(batch.load, WindowLoad::Returned { .. }) {
                break batch;
            }
        };
        assert_eq!(loaded.rows.len(), 2);
        assert_eq!(loaded.load, WindowLoad::Returned { added: 1 });

        let (idle_ready, idle_started) = mpsc::channel();
        let (idle_result, idle_done) = mpsc::channel();
        let idle_waiter = thread::spawn(move || {
            idle_ready.send(()).unwrap();
            idle_result.send(receiver.recv().is_err()).unwrap();
        });
        idle_started.recv().unwrap();
        handle.unsubscribe_history(history_handle);
        assert!(idle_done.recv_timeout(Duration::from_secs(1)).unwrap());
        idle_waiter.join().unwrap();

        let (_shutdown_handle, shutdown_receiver) = handle.subscribe_history(query).unwrap();
        shutdown_receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        let (shutdown_ready, shutdown_started) = mpsc::channel();
        let (shutdown_result, shutdown_done) = mpsc::channel();
        let shutdown_waiter = thread::spawn(move || {
            shutdown_ready.send(()).unwrap();
            shutdown_result
                .send(shutdown_receiver.recv().is_err())
                .unwrap();
        });
        shutdown_started.recv().unwrap();
        handle.shutdown();
        engine_thread.join();
        assert!(shutdown_done.recv_timeout(Duration::from_secs(1)).unwrap());
        shutdown_waiter.join().unwrap();
    }
}

/// The app-facing handle to a live subscription (returned by
/// [`Handle::subscribe`]). `Send`, `Copy`-cheap, carries nothing that
/// borrows into the engine thread — it is exactly the correlation id
/// [`Handle::unsubscribe`] needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryHandle(HandleId);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryHandle(HistorySessionId);

/// A newly accepted write's stable store-issued identity plus its live
/// observer. Keeping the id separate from the channel lets a later process
/// call [`Handle::reattach_receipt`] without replaying acceptance.
pub struct ReceiptStream {
    pub id: ReceiptId,
    pub statuses: Receiver<WriteStatus>,
}

/// Result of looking up retained receipt facts by stable id.
pub enum ReceiptReattachment {
    /// The observer is attached and this channel is already primed with all
    /// readable retained facts.
    Attached(Receiver<WriteStatus>),
    /// No retained receipt with this id exists.
    NotFound,
    /// The id is retained, but durable receipt or attempt evidence is corrupt
    /// or unreadable. The obligation remains untouched and nothing publishes.
    RetainedButUnreadable,
}

/// A `RowSink` that intentionally does nothing: rows+coverage are delivered
/// from `Effect::EmitRows` instead (see the module doc). `EngineCore`'s
/// `Subscribe` still requires a sink object to satisfy its own bookkeeping
/// (`HandleState::sink`) — this is that placeholder, never a second
/// delivery path.
struct NullRowSink;

impl RowSink for NullRowSink {
    fn on_rows(&self, _rows: Vec<RowDelta>) {}
}

struct NullHistorySink;

impl HistorySink for NullHistorySink {
    fn on_history(&self, _batch: HistoryBatch) {}
}

/// Forwards every `WriteStatus` `EngineCore` reports straight onto the
/// caller's channel. This IS the receipt delivery path (see the module doc):
/// `Effect::EmitReceipt` carries the identical value and is not separately
/// redelivered.
struct ChannelReceiptSink(Sender<WriteStatus>);

impl ReceiptSink for ChannelReceiptSink {
    fn on_status(&self, status: WriteStatus) {
        let _ = self.0.send(status);
    }
}

fn publish_result(effects: &[Effect]) -> Result<ReceiptId, PublishError> {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitReceipt(id, _) => Some(Ok(*id)),
            Effect::PublishFailed(err) => Some(Err(*err)),
            _ => None,
        })
        .expect("every publish produces a receipt id or typed allocation failure")
}

#[cfg(test)]
mod publish_result_tests {
    use super::*;

    #[test]
    fn typed_pre_receipt_failure_is_the_publish_reply() {
        assert_eq!(
            publish_result(&[Effect::PublishFailed(
                PublishError::ReceiptCorrelationIdExhausted,
            )]),
            Err(PublishError::ReceiptCorrelationIdExhausted)
        );
        assert_eq!(
            publish_result(&[Effect::EmitReceipt(
                ReceiptId(1u64 << 63),
                WriteStatus::Failed("rejected".to_string()),
            )]),
            Ok(ReceiptId(1u64 << 63))
        );
    }
}

/// The runtime-private envelope the engine thread's blocking recv loop reads.
/// `Engine` carries the plain reducer vocabulary (`core::EngineMsg`) exactly
/// as-is — this is what pool-translated relay events, signer completions,
/// `Unsubscribe`/`SetActivePubkey`/`Publish` all travel as. `Subscribe` is
/// the one verb that needs a synchronous reply: the caller cannot construct
/// a `QueryHandle` (nor start reading rows) until it knows the `HandleId`
/// `EngineCore` assigns, which only exists after `EngineCore::handle` has
/// already run — so the reply carries both the id and the row channel back
/// in one round trip. `Shutdown` stops the loop; the engine thread tears
/// down its own `Pool` clone on the way out (see `spawn`).
enum Cmd {
    Engine(EngineMsg),
    RelayInformationFetched {
        url: RelayUrl,
        generation: u64,
        result: Box<Result<RelayInformationSnapshot, RelayInformationError>>,
    },
    /// One ordered relay batch plus an applied acknowledgement. The bridge
    /// waits for this acknowledgement before draining another frame batch,
    /// propagating store/engine pressure back into the bounded pool queues.
    RelayBatch {
        frames: Vec<(
            nmp_transport::RelayHandle,
            RelaySessionKey,
            nmp_transport::RelayFrame,
        )>,
        applied: cb::Sender<()>,
    },
    /// A closed relay OS thread has been joined and the finite retirement
    /// envelope has capacity again. Reconcile exact required demand once;
    /// this event edge replaces polling or a retry spin.
    RelayWorkerRetired,
    Subscribe {
        query: LiveQuery,
        reply: Sender<Result<(HandleId, Receiver<RowsMsg>), EngineThreadError>>,
    },
    SubscribeHistory {
        query: HistoryQuery,
        reply: Sender<Result<(HistorySessionId, HistoryReceiver), EngineThreadError>>,
    },
    RequestRows {
        id: HistorySessionId,
        at_least: usize,
        reply: Sender<Result<(), HistoryAdvanceError>>,
    },
    UnsubscribeHistory(HistorySessionId),
    PublishTracked {
        intent: WriteIntent,
        sink: Box<dyn ReceiptSink>,
        reply: Sender<Result<ReceiptId, PublishError>>,
    },
    ReattachReceipt {
        id: ReceiptId,
        sink: Box<dyn ReceiptSink>,
        reply: Sender<ReattachOutcome>,
    },
    /// Register a new signing capability (M4 §5: `SignerRegistry`). The
    /// reply carries the pubkey the engine thread's registry keyed it under,
    /// or a typed error if the capability has no stable identity.
    AddSigner {
        signer: Box<dyn SigningCapability + Send>,
        reply: Sender<Result<SignerRegistration, AddSignerError>>,
    },
    RemoveSigner {
        registration: SignerRegistration,
        reply: Sender<bool>,
    },
    /// Sign one exact event through the active account's registered
    /// capability without entering the write/store/outbox reducer.
    SignEvent {
        unsigned: UnsignedEvent,
        reservation: nmp_executor::Reservation,
        completion: SignEventCompletion,
        reply: Sender<Result<SignEventRegistration, SignEventError>>,
    },
    CancelSignEvent(u64),
    SignEventFinished(u64),
    /// Register a new diagnostics observer (M5 plan §1.2 step 4). The reply
    /// carries the id (used only by `Cmd::UnobserveDiagnostics` to withdraw
    /// later) and a mailbox already primed with the CURRENT snapshot — an
    /// observer that registers between recompiles should not have to wait
    /// for the next one to see anything (mirrors `Cmd::Subscribe`'s own
    /// immediate first `EmitRows`).
    ObserveDiagnostics {
        reply: Sender<(u64, LatestReceiver<DiagnosticsSnapshot>)>,
    },
    /// Withdraw a diagnostics observer registered via `ObserveDiagnostics`.
    /// Fire-and-forget, same discipline as `Cmd::Engine(EngineMsg::
    /// Unsubscribe(..))`: dropping the registry's `LatestSender` is what
    /// lets the observer's `LatestReceiver::recv` return `None`.
    UnobserveDiagnostics(u64),
    Shutdown,
}

/// Every signing capability the engine thread currently holds, keyed by its
/// own public key. `Effect::RequestSign` resolves the exact pubkey frozen in
/// the accepted template; mutable active-account state can never redirect
/// already-accepted work.
#[derive(Default)]
struct SignerRegistry {
    signers: HashMap<PublicKey, RegisteredSigner>,
}

/// Typed outcome vocabulary for the governed sign-only operation. This is
/// deliberately separate from write receipts: signing here never accepts a
/// write intent, mutates canonical storage, creates an outbox lane, or
/// publishes to a relay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignEventError {
    NoActiveSigner,
    InvalidRequest { reason: String },
    SignerUnavailable { reason: String },
    SignerRejected { reason: String },
    InvalidSignerOutput { reason: String },
    ExecutorSaturated { capacity: usize },
    ThreadUnavailable { component: String, reason: String },
    EngineClosed,
    Cancelled,
}

type SignEventCompletion = Box<dyn FnOnce(Result<SignedEvent, SignEventError>) + Send + 'static>;

const SIGN_EVENT_OPEN: u8 = 0;
const SIGN_EVENT_CANCELLED: u8 = 1;
const SIGN_EVENT_RESOLVED: u8 = 2;

/// One linearization point shared by caller cancellation, engine shutdown,
/// executor shutdown, and signer completion. Only the admitted worker owns
/// the foreign completion; cancellation merely claims `Open -> Cancelled`,
/// wakes that worker, and releases an optional adapter RPC.
struct SignEventTerminal {
    state: AtomicU8,
    cancel: PendingSignerCancel,
}

impl SignEventTerminal {
    fn new() -> (Arc<Self>, PendingSignerCancelled) {
        let (cancel, cancelled) = pending_signer_cancellation();
        (
            Arc::new(Self {
                state: AtomicU8::new(SIGN_EVENT_OPEN),
                cancel,
            }),
            cancelled,
        )
    }

    fn cancel(&self) -> bool {
        if self
            .state
            .compare_exchange(
                SIGN_EVENT_OPEN,
                SIGN_EVENT_CANCELLED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return false;
        }
        self.cancel.cancel();
        true
    }

    fn resolve(&self) -> bool {
        self.state
            .compare_exchange(
                SIGN_EVENT_OPEN,
                SIGN_EVENT_RESOLVED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }
}

struct SignEventRegistration {
    id: u64,
    terminal: Arc<SignEventTerminal>,
}

impl std::fmt::Display for SignEventError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoActiveSigner => f.write_str("the active account has no registered signer"),
            Self::InvalidRequest { reason } => write!(f, "invalid sign request: {reason}"),
            Self::SignerUnavailable { reason } => write!(f, "signer unavailable: {reason}"),
            Self::SignerRejected { reason } => write!(f, "signer rejected request: {reason}"),
            Self::InvalidSignerOutput { reason } => {
                write!(f, "signer returned invalid output: {reason}")
            }
            Self::ExecutorSaturated { capacity } => {
                write!(
                    f,
                    "sign-event refused: native task executor is at capacity {capacity}"
                )
            }
            Self::ThreadUnavailable { component, reason } => {
                write!(f, "{component} thread unavailable: {reason}")
            }
            Self::EngineClosed => f.write_str("engine already shut down"),
            Self::Cancelled => f.write_str("sign operation cancelled"),
        }
    }
}

impl std::error::Error for SignEventError {}

fn signer_error(error: nmp_signer::SignerError) -> SignEventError {
    match error {
        nmp_signer::SignerError::InvalidResponse(reason) => {
            SignEventError::InvalidSignerOutput { reason }
        }
        nmp_signer::SignerError::Rejected(reason) => SignEventError::SignerRejected { reason },
        other => SignEventError::SignerUnavailable {
            reason: other.to_string(),
        },
    }
}

fn validate_sign_request(unsigned: &UnsignedEvent) -> Result<EventId, SignEventError> {
    let computed = EventId::new(
        &unsigned.pubkey,
        &unsigned.created_at,
        &unsigned.kind,
        &unsigned.tags,
        &unsigned.content,
    );
    if unsigned.id.is_some_and(|declared| declared != computed) {
        return Err(SignEventError::InvalidRequest {
            reason: "declared event id does not match the immutable body".to_string(),
        });
    }
    Ok(computed)
}

fn validate_signer_output(
    unsigned: &UnsignedEvent,
    expected_id: EventId,
    signed: SignedEvent,
) -> Result<SignedEvent, SignEventError> {
    if signed.id != expected_id
        || signed.pubkey != unsigned.pubkey
        || signed.created_at != unsigned.created_at
        || signed.kind != unsigned.kind
        || signed.tags != unsigned.tags
        || signed.content != unsigned.content
    {
        return Err(SignEventError::InvalidSignerOutput {
            reason: "signed event does not match the frozen body, author, or id".to_string(),
        });
    }
    signed
        .verify()
        .map_err(|error| SignEventError::InvalidSignerOutput {
            reason: format!("signature verification failed: {error}"),
        })?;
    Ok(signed)
}

struct RegisteredSigner {
    identity: Arc<()>,
    signer: Box<dyn SigningCapability + Send>,
}

impl SignerRegistry {
    /// Register `signer` under its own `public_key()`, replacing any prior
    /// capability already registered for that key.
    fn add(
        &mut self,
        signer: Box<dyn SigningCapability + Send>,
    ) -> Result<SignerRegistration, AddSignerError> {
        let pk = signer
            .public_key()
            .ok_or(AddSignerError::MissingPublicKey)?;
        let identity = Arc::new(());
        self.signers.insert(
            pk,
            RegisteredSigner {
                identity: Arc::clone(&identity),
                signer,
            },
        );
        Ok(SignerRegistration {
            public_key: pk,
            identity,
        })
    }

    /// Remove only the capability installed by this exact registration.
    /// A stale remote session can therefore never detach a newer replacement
    /// for the same account.
    fn remove(&mut self, registration: &SignerRegistration) -> bool {
        let is_current = self
            .signers
            .get(&registration.public_key)
            .is_some_and(|current| Arc::ptr_eq(&current.identity, &registration.identity));
        if is_current {
            self.signers.remove(&registration.public_key);
        }
        is_current
    }

    /// Resolve the signer frozen into this exact accepted template. An
    /// account switch cannot redirect already-accepted work.
    fn signer_for(&self, pk: PublicKey) -> Option<&(dyn SigningCapability + Send)> {
        self.signers.get(&pk).map(|entry| entry.signer.as_ref())
    }
}

/// One dedicated engine OS thread (§2 position 2) plus the pool-bridge
/// thread that feeds it. Returned alongside the [`Handle`] the app actually
/// uses; kept around only so a caller (chiefly tests) can deterministically
/// `join` both threads after triggering [`Handle::shutdown`].
pub struct EngineThread {
    engine_join: Option<JoinHandle<()>>,
    bridge_join: Option<JoinHandle<()>>,
    native_tasks: nmp_executor::Executor,
}

#[cfg(test)]
static ACTIVE_RUNTIME_THREADS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
static RUNTIME_LIFECYCLE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
struct RuntimeThreadCountGuard;

#[cfg(test)]
impl RuntimeThreadCountGuard {
    fn enter() -> Self {
        ACTIVE_RUNTIME_THREADS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self
    }
}

#[cfg(test)]
impl Drop for RuntimeThreadCountGuard {
    fn drop(&mut self) {
        ACTIVE_RUNTIME_THREADS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Supported construction failure for the engine-owned thread graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineThreadError {
    ThreadUnavailable { component: String, reason: String },
    RelayBudgetOverflow { relay_limit: usize },
}

impl std::fmt::Display for EngineThreadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ThreadUnavailable { component, reason } => {
                write!(f, "{component} thread unavailable: {reason}")
            }
            Self::RelayBudgetOverflow { relay_limit } => write!(
                f,
                "relay worker budget {relay_limit} cannot represent its retirement envelope"
            ),
        }
    }
}

impl std::error::Error for EngineThreadError {}

fn pool_build_error(error: nmp_transport::PoolBuildError) -> EngineThreadError {
    match error {
        nmp_transport::PoolBuildError::ThreadUnavailable(error) => {
            EngineThreadError::ThreadUnavailable {
                component: error.role.to_string(),
                reason: error.reason,
            }
        }
        nmp_transport::PoolBuildError::RelayBudgetOverflow { max_relays } => {
            EngineThreadError::RelayBudgetOverflow {
                relay_limit: max_relays,
            }
        }
    }
}

impl EngineThread {
    /// Spawn the engine thread + the pool-bridge thread. `store`/`directory`
    /// are constructed by the CALLER but moved whole into the engine
    /// thread's closure and built into `EngineCore` there — they never cross
    /// back out, which is what lets `EngineCore` itself stay `!Send`-friendly
    /// (only `Send + 'static` values ever cross the thread boundary, exactly
    /// once, at spawn time). The engine starts with an EMPTY `SignerRegistry`
    /// (zero accounts, read-only) — matching a logged-out launch (M4 §5);
    /// the caller registers accounts afterward via [`Handle::add_signer`] and
    /// picks one via [`Handle::set_active_account`].
    pub fn spawn<S, D>(
        store: S,
        directory: D,
        cap: usize,
        pool_config: PoolConfig,
        admission: RelayAdmissionPolicy,
    ) -> Result<(Self, Handle), EngineThreadError>
    where
        S: EventStore + Send + 'static,
        D: RelayDirectory + Send + 'static,
    {
        Self::spawn_with_native_task_limit(
            store,
            directory,
            cap,
            pool_config,
            admission,
            nmp_executor::DEFAULT_MAX_TASKS,
        )
    }

    pub fn spawn_with_native_task_limit<S, D>(
        store: S,
        directory: D,
        cap: usize,
        mut pool_config: PoolConfig,
        admission: RelayAdmissionPolicy,
        max_native_tasks: usize,
    ) -> Result<(Self, Handle), EngineThreadError>
    where
        S: EventStore + Send + 'static,
        D: RelayDirectory + Send + 'static,
    {
        let native_tasks = nmp_executor::Executor::new(max_native_tasks).map_err(|error| {
            EngineThreadError::ThreadUnavailable {
                component: "native task executor".to_string(),
                reason: error.to_string(),
            }
        })?;
        // One limit owns both compilation and connection admission. Legacy
        // zero values select the finite default; conflicting mechanism-test
        // inputs fail closed to the smaller non-zero ceiling.
        let cap = match (cap, pool_config.max_relays) {
            (0, 0) => nmp_transport::DEFAULT_MAX_RELAYS,
            (0, pool) => pool,
            (router, 0) => router,
            (router, pool) => router.min(pool),
        };
        pool_config.max_relays = cap;
        // Issue #519: thread the SAME opt-in local-host allowlist this
        // `admission` policy enforces at discovery-time into both places
        // that actually open a socket/DNS-resolve a relay, so an operator's
        // intentional local relay keeps working after resolved-IP admission
        // (`pool::connect`'s dial, `HttpFetcher`'s NIP-11 resolver) is
        // enforced there too — see those modules' docs for why the URL
        // string alone is never sufficient.
        let allowed_local_hosts: Arc<BTreeSet<String>> =
            Arc::new(admission.allowed_local_hosts().clone());
        pool_config.allowed_local_hosts = Arc::clone(&allowed_local_hosts);

        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let relay_information = RelayInformationService::new_with_admission(
            native_tasks.clone(),
            Arc::clone(&allowed_local_hosts),
        );
        let max_engine_batch = pool_config.max_engine_batch.max(1);
        let (pool_evt_tx, pool_evt_rx) =
            cb::bounded::<PoolEvent>(pool_config.event_sink_queue_capacity.max(1));
        let (pool_stop_tx, pool_stop_rx) = cb::bounded::<()>(0);
        // The pool's OWN mio worker threads + translator thread are interior
        // to `Pool` (harvested, HARVEST-justified in nmp-transport's own
        // docs) — this crate never touches mio/tungstenite directly.
        let pool = match Pool::new(
            pool_config,
            EnginePoolSink {
                events: pool_evt_tx,
                stopping: pool_stop_rx.clone(),
            },
        ) {
            Ok(pool) => pool,
            Err(error) => {
                native_tasks.shutdown();
                return Err(pool_build_error(error));
            }
        };

        let bridge_inbox = cmd_tx.clone();
        let bridge_join = match thread::Builder::new()
            .name("nmp-engine-pool-bridge".to_string())
            .spawn(move || {
                #[cfg(test)]
                let _thread_count = RuntimeThreadCountGuard::enter();
                pool_bridge_loop(&pool_evt_rx, &pool_stop_rx, &bridge_inbox, max_engine_batch)
            }) {
            Ok(join) => join,
            Err(error) => {
                pool.shutdown();
                native_tasks.shutdown();
                return Err(EngineThreadError::ThreadUnavailable {
                    component: "engine pool bridge".to_string(),
                    reason: error.to_string(),
                });
            }
        };

        let self_inbox = cmd_tx.clone();
        let engine_pool = pool.clone();
        let engine_stop = pool_stop_tx.clone();
        let engine_native_tasks = native_tasks.clone();
        let engine_relay_information = relay_information.clone();
        let engine_join =
            match thread::Builder::new()
                .name("nmp-engine".to_string())
                .spawn(move || {
                    #[cfg(test)]
                    let _thread_count = RuntimeThreadCountGuard::enter();
                    engine_loop(
                        store,
                        directory,
                        cap,
                        admission,
                        EnginePoolRuntime {
                            pool: engine_pool,
                            stop: engine_stop,
                            native_tasks: engine_native_tasks,
                            relay_information: engine_relay_information,
                        },
                        &cmd_rx,
                        &self_inbox,
                    )
                }) {
                Ok(join) => join,
                Err(error) => {
                    drop(pool_stop_tx);
                    pool.shutdown();
                    let _ = bridge_join.join();
                    native_tasks.shutdown();
                    return Err(EngineThreadError::ThreadUnavailable {
                        component: "engine runtime".to_string(),
                        reason: error.to_string(),
                    });
                }
            };
        drop(pool);

        let handle_native_tasks = native_tasks.clone();
        Ok((
            Self {
                engine_join: Some(engine_join),
                bridge_join: Some(bridge_join),
                native_tasks,
            },
            Handle {
                inbox: cmd_tx,
                native_tasks: handle_native_tasks,
                relay_information,
            },
        ))
    }

    /// Block until both the engine thread and the pool-bridge thread have
    /// exited. Only returns once a [`Handle::shutdown`] has actually been
    /// observed by the engine thread (which then tears down its `Pool`
    /// clone, which is what lets the bridge thread's `recv` finally see a
    /// disconnected channel and exit in turn) — callers that never shut down
    /// any `Handle` will block here forever, matching `Pool::shutdown`'s own
    /// join discipline.
    pub fn join(mut self) {
        if let Some(h) = self.engine_join.take() {
            let _ = h.join();
        }
        if let Some(h) = self.bridge_join.take() {
            let _ = h.join();
        }
        self.native_tasks.shutdown();
    }

    pub fn native_tasks(&self) -> nmp_executor::Executor {
        self.native_tasks.clone()
    }
}

/// Wall-clock `Duration` from `now` until `deadline` (§3.3's `recv_timeout`
/// argument), floored at zero for a deadline already past -- the "a past-due
/// deadline yields a zero timeout -> immediate tick" case (boot-time
/// catch-up on a persisted expiration index, or simply losing a race with
/// the wall clock between `next_deadline()` and this call). `Timestamp` is
/// second-resolution (NIP-40's own unit -- every deadline source
/// `EngineCore::next_deadline` folds in is that same resolution), so this
/// loop's wake precision is bounded by a second, never finer.
fn duration_until(deadline: Timestamp, now: Timestamp) -> Duration {
    if deadline <= now {
        Duration::ZERO
    } else {
        Duration::from_secs(deadline.as_secs().saturating_sub(now.as_secs()))
    }
}

/// Blocking translator loop (D8): `PoolEvent` -> `EngineMsg` -> the engine
/// thread's inbox. Exits as soon as `pool_evt_rx` disconnects, which only
/// happens once every clone of the pool's sink is gone (see `EngineThread::
/// join`'s doc).
fn pool_bridge_loop(
    pool_evt_rx: &cb::Receiver<PoolEvent>,
    stopping: &cb::Receiver<()>,
    engine_inbox: &Sender<Cmd>,
    max_engine_batch: usize,
) {
    loop {
        let event = cb::select_biased! {
            recv(stopping) -> _ => break,
            recv(pool_evt_rx) -> event => match event {
                Ok(event) => event,
                Err(_) => break,
            },
        };
        if let PoolEvent::Frame {
            handle,
            session,
            frame,
        } = event
        {
            let mut frames = vec![(handle, session, frame)];
            let trailing = loop {
                if frames.len() == max_engine_batch {
                    break None;
                }
                match pool_evt_rx.try_recv() {
                    Ok(PoolEvent::Frame {
                        handle,
                        session,
                        frame,
                    }) => frames.push((handle, session, frame)),
                    Ok(other) => break Some(other),
                    Err(cb::TryRecvError::Empty | cb::TryRecvError::Disconnected) => break None,
                }
            };
            let (applied_tx, applied_rx) = cb::bounded(1);
            if engine_inbox
                .send(Cmd::RelayBatch {
                    frames,
                    applied: applied_tx,
                })
                .is_err()
            {
                break;
            }
            let applied = cb::select_biased! {
                recv(stopping) -> _ => false,
                recv(applied_rx) -> result => result.is_ok(),
            };
            if !applied {
                break;
            }
            if let Some(trailing) = trailing {
                if !forward_pool_event(trailing, engine_inbox) {
                    break;
                }
            }
            continue;
        }
        if !forward_pool_event(event, engine_inbox) {
            break; // engine thread is gone; nothing left to feed.
        }
    }
}

fn forward_pool_event(event: PoolEvent, engine_inbox: &Sender<Cmd>) -> bool {
    match event {
        PoolEvent::WorkerRetired => engine_inbox.send(Cmd::RelayWorkerRetired).is_ok(),
        event => translate_pool_event(event)
            .is_none_or(|message| engine_inbox.send(Cmd::Engine(message)).is_ok()),
    }
}

#[cfg(test)]
mod pool_bridge_tests {
    use super::*;
    use nmp_transport::{PoolEventSink, RelayFrame, RelayHandle};
    use nostr::RelayMessage;

    fn notice_frame(text: &str) -> RelayFrame {
        RelayFrame::from_message(RelayMessage::notice(text))
    }

    fn test_session() -> RelaySessionKey {
        RelaySessionKey::public(RelayUrl::parse("wss://relay.example.com").unwrap())
    }

    #[test]
    fn bridge_waits_for_applied_ack_before_enqueuing_another_relay_batch() {
        let (pool_tx, pool_rx) = cb::bounded(8);
        let (stop_tx, stop_rx) = cb::bounded(0);
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let bridge = thread::spawn(move || pool_bridge_loop(&pool_rx, &stop_rx, &cmd_tx, 128));
        let handle = RelayHandle {
            slot: 1,
            generation: 2,
        };

        pool_tx
            .send(PoolEvent::Frame {
                handle,
                session: test_session(),
                frame: notice_frame("first"),
            })
            .unwrap();
        let first_ack = match cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            Cmd::RelayBatch { frames, applied } => {
                assert_eq!(frames.len(), 1);
                applied
            }
            _ => panic!("bridge must emit a relay batch"),
        };

        pool_tx
            .send(PoolEvent::Frame {
                handle,
                session: test_session(),
                frame: notice_frame("second"),
            })
            .unwrap();
        assert!(
            matches!(cmd_rx.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "a second relay batch cannot enter the engine inbox before ack"
        );

        first_ack.send(()).unwrap();
        let second_ack = match cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            Cmd::RelayBatch { frames, applied } => {
                assert_eq!(frames.len(), 1);
                applied
            }
            _ => panic!("bridge must emit the second relay batch after ack"),
        };
        second_ack.send(()).unwrap();
        drop(pool_tx);
        drop(stop_tx);
        bridge.join().unwrap();
    }

    #[test]
    fn bridge_caps_each_engine_transaction_without_losing_order() {
        let (pool_tx, pool_rx) = cb::bounded(8);
        let (stop_tx, stop_rx) = cb::bounded(0);
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let handle = RelayHandle {
            slot: 1,
            generation: 2,
        };
        for text in ["one", "two", "three"] {
            pool_tx
                .send(PoolEvent::Frame {
                    handle,
                    session: test_session(),
                    frame: notice_frame(text),
                })
                .unwrap();
        }
        let bridge = thread::spawn(move || pool_bridge_loop(&pool_rx, &stop_rx, &cmd_tx, 2));

        let first_ack = match cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            Cmd::RelayBatch { frames, applied } => {
                assert_eq!(frames.len(), 2);
                assert_eq!(
                    frames[0].2.clone().into_message(),
                    RelayMessage::notice("one")
                );
                assert_eq!(
                    frames[1].2.clone().into_message(),
                    RelayMessage::notice("two")
                );
                applied
            }
            _ => panic!("first command must be a capped relay batch"),
        };
        first_ack.send(()).unwrap();
        let second_ack = match cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            Cmd::RelayBatch { frames, applied } => {
                assert_eq!(frames.len(), 1);
                assert_eq!(
                    frames[0].2.clone().into_message(),
                    RelayMessage::notice("three")
                );
                applied
            }
            _ => panic!("second command must retain the next ordered frame"),
        };
        second_ack.send(()).unwrap();
        drop(pool_tx);
        drop(stop_tx);
        bridge.join().unwrap();
    }

    #[test]
    fn stop_disconnect_releases_bridge_waiting_for_engine_ack() {
        let (pool_tx, pool_rx) = cb::bounded(1);
        let (stop_tx, stop_rx) = cb::bounded(0);
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let bridge = thread::spawn(move || pool_bridge_loop(&pool_rx, &stop_rx, &cmd_tx, 1));
        pool_tx
            .send(PoolEvent::Frame {
                handle: RelayHandle {
                    slot: 1,
                    generation: 2,
                },
                session: test_session(),
                frame: notice_frame("pending"),
            })
            .unwrap();
        let _unacked = cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        drop(stop_tx);
        bridge.join().unwrap();
        drop(pool_tx);
    }

    #[test]
    fn bounded_pool_sink_is_cancelled_without_polling_during_shutdown() {
        let (events_tx, events_rx) = cb::bounded(1);
        let (stop_tx, stop_rx) = cb::bounded(0);
        let sink = EnginePoolSink {
            events: events_tx,
            stopping: stop_rx,
        };
        sink.on_event(PoolEvent::Disconnected {
            handle: RelayHandle {
                slot: 1,
                generation: 1,
            },
            session: test_session(),
            reason: nmp_transport::DisconnectReason::Error,
        });
        let blocked = thread::spawn(move || {
            sink.on_event(PoolEvent::Disconnected {
                handle: RelayHandle {
                    slot: 2,
                    generation: 1,
                },
                session: test_session(),
                reason: nmp_transport::DisconnectReason::Error,
            });
        });

        drop(stop_tx);
        blocked.join().unwrap();
        assert_eq!(events_rx.len(), 1, "shutdown does not enqueue a tail");
    }
}

#[cfg(test)]
// The closed-surface falsifier scans this module's code lines for the token
// `relays:`. Assigning the cap after `Default` keeps a pool fixture from
// masquerading as a forbidden bare-relay method parameter in that scan.
#[allow(clippy::field_reassign_with_default)]
mod relay_worker_reconciliation_tests {
    use super::*;
    use std::collections::BTreeSet;

    use nmp_grammar::{Binding, Durability, Filter, WriteIntent, WritePayload, WriteRouting};
    use nmp_router::FixtureDirectory;
    use nmp_store::MemoryStore;
    use nostr::{Keys, Kind, UnsignedEvent};

    struct NullReceiptSink;

    impl ReceiptSink for NullReceiptSink {
        fn on_status(&self, _status: WriteStatus) {}
    }

    fn query(author: &str) -> LiveQuery {
        LiveQuery::from_filter(Filter {
            kinds: Some(BTreeSet::from([1])),
            authors: Some(Binding::Literal(BTreeSet::from([author.to_string()]))),
            ..Filter::default()
        })
    }

    #[test]
    fn repeated_engine_shutdown_returns_runtime_threads_to_exact_baseline() {
        let _serial = RUNTIME_LIFECYCLE_TEST_LOCK.lock().unwrap();
        let baseline = ACTIVE_RUNTIME_THREADS.load(std::sync::atomic::Ordering::SeqCst);
        for _ in 0..16 {
            let (engine, handle) = EngineThread::spawn(
                MemoryStore::new(),
                FixtureDirectory::new(),
                1,
                PoolConfig::default(),
                RelayAdmissionPolicy::default(),
            )
            .expect("engine construction");
            handle.shutdown();
            engine.join();
            assert_eq!(
                ACTIVE_RUNTIME_THREADS.load(std::sync::atomic::Ordering::SeqCst),
                baseline,
                "join must be an exact engine/bridge teardown barrier"
            );
        }
    }

    /// #20 churn falsifier: a cap-sized old plan must release its historical
    /// worker set before a disjoint replacement plan dials. Before exact
    /// reconciliation, the first worker stayed live after its last `CLOSE`,
    /// so the replacement `ensure_open` was refused forever even though the
    /// current router plan itself contained exactly one relay under cap=1.
    #[test]
    fn cap_sized_plan_can_replace_every_relay_without_stranding_new_demand() {
        let author_a = "aa".repeat(32);
        let author_b = "bb".repeat(32);
        let relay_a = RelayUrl::parse("wss://relay-a.example").unwrap();
        let relay_b = RelayUrl::parse("wss://relay-b.example").unwrap();
        let directory = FixtureDirectory::new()
            .with_write(author_a.clone(), [relay_a.clone()])
            .with_write(author_b.clone(), [relay_b.clone()]);
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(directory), 1);
        let (pool_tx, _pool_rx) = mpsc::channel();
        let mut config = PoolConfig::default();
        config.max_relays = 1;
        let pool = Pool::new(config, pool_tx).expect("test pool construction");
        let mut rows = HashMap::new();
        let mut histories = HashMap::new();
        let mut diagnostics = HashMap::new();
        let mut preambles = Preambles::new();
        let registry = SignerRegistry::default();
        let (self_inbox, _inbox_rx) = mpsc::channel();
        let native_tasks = nmp_executor::Executor::new(1).unwrap();
        let relay_information = RelayInformationService::new(native_tasks.clone());
        let nip11_decisions = RefCell::new(Nip11DecisionState::default());
        let dispatch_runtime = DispatchRuntime {
            self_inbox: &self_inbox,
            relay_information: &relay_information,
            native_tasks: &native_tasks,
            nip11_decisions: &nip11_decisions,
        };

        let first = core.handle(EngineMsg::Subscribe(
            query(&author_a),
            Box::new(NullRowSink),
        ));
        let first_id = first
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitRows(id, ..) => Some(*id),
                _ => None,
            })
            .expect("subscription emits its initial rows");
        dispatch_core_effects(
            &core,
            first,
            &pool,
            &mut rows,
            &mut histories,
            &mut diagnostics,
            &mut preambles,
            &registry,
            dispatch_runtime,
        );
        assert!(pool.live_handle(&relay_a).is_some());

        let withdrawn = core.handle(EngineMsg::Unsubscribe(first_id));
        dispatch_core_effects(
            &core,
            withdrawn,
            &pool,
            &mut rows,
            &mut histories,
            &mut diagnostics,
            &mut preambles,
            &registry,
            dispatch_runtime,
        );
        assert!(
            pool.live_handle(&relay_a).is_none(),
            "a relay with no read or write owner must release its slot"
        );

        let replacement = core.handle(EngineMsg::Subscribe(
            query(&author_b),
            Box::new(NullRowSink),
        ));
        dispatch_core_effects(
            &core,
            replacement,
            &pool,
            &mut rows,
            &mut histories,
            &mut diagnostics,
            &mut preambles,
            &registry,
            dispatch_runtime,
        );
        assert!(
            pool.live_handle(&relay_b).is_some(),
            "the in-budget replacement relay must acquire the freed slot"
        );
        assert_eq!(
            pool.admission_rejections(),
            0,
            "correct release ordering must avoid a transient cap refusal"
        );

        pool.shutdown();
        native_tasks.shutdown();
    }

    /// Exact read reconciliation must not evict a worker owned only by a
    /// durable write lane. A socket is shared transport state: releasing it
    /// from the router plan is safe only after every nonterminal outbox lane
    /// for that relay is also gone.
    #[test]
    fn durable_write_lane_retains_worker_without_read_demand() {
        let author = Keys::generate();
        let relay = RelayUrl::parse("wss://write-only.example").unwrap();
        // #8 U1: the write plane rides the relay's PUBLIC session (no AUTH
        // reducer yet), so the worker this durable lane owns is the public
        // session for the URL. Authenticated per-identity write sessions
        // arrive with the AUTH wave.
        let write_session = RelaySessionKey::public(relay.clone());
        let directory =
            FixtureDirectory::new().with_write(author.public_key().to_hex(), [relay.clone()]);
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(directory), 1);
        core.handle(EngineMsg::SetActivePubkey(Some(author.public_key())));

        let unsigned = UnsignedEvent::new(
            author.public_key(),
            Timestamp::from(1),
            Kind::TextNote,
            Vec::new(),
            "write owns its worker",
        );
        let accepted = core.handle(EngineMsg::Publish(
            WriteIntent {
                payload: WritePayload::Unsigned(unsigned),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
            },
            Box::new(NullReceiptSink),
        ));
        let (receipt_id, generation, unsigned) = accepted
            .into_iter()
            .find_map(|effect| match effect {
                Effect::RequestSign(id, generation, unsigned) => Some((id, generation, unsigned)),
                _ => None,
            })
            .expect("durable unsigned write requests signing");
        let signed = unsigned.sign_with_keys(&author).unwrap();
        let ready = core.handle(EngineMsg::SignerCompleted(
            receipt_id,
            generation,
            Ok(signed),
        ));

        let (pool_tx, _pool_rx) = mpsc::channel();
        let mut config = PoolConfig::default();
        config.max_relays = 1;
        let pool = Pool::new(config, pool_tx).expect("test pool construction");
        let mut rows = HashMap::new();
        let mut histories = HashMap::new();
        let mut diagnostics = HashMap::new();
        let mut preambles = Preambles::new();
        let registry = SignerRegistry::default();
        let (self_inbox, _inbox_rx) = mpsc::channel();
        let native_tasks = nmp_executor::Executor::new(1).unwrap();
        let relay_information = RelayInformationService::new(native_tasks.clone());
        let nip11_decisions = RefCell::new(Nip11DecisionState::default());
        let dispatch_runtime = DispatchRuntime {
            self_inbox: &self_inbox,
            relay_information: &relay_information,
            native_tasks: &native_tasks,
            nip11_decisions: &nip11_decisions,
        };

        dispatch_core_effects(
            &core,
            ready,
            &pool,
            &mut rows,
            &mut histories,
            &mut diagnostics,
            &mut preambles,
            &registry,
            dispatch_runtime,
        );
        assert!(pool.live_session_handle(&write_session).is_some());

        dispatch_core_effects(
            &core,
            Vec::new(),
            &pool,
            &mut rows,
            &mut histories,
            &mut diagnostics,
            &mut preambles,
            &registry,
            dispatch_runtime,
        );
        assert!(
            pool.live_session_handle(&write_session).is_some(),
            "a nonterminal durable lane remains a worker owner"
        );

        pool.shutdown();
        native_tasks.shutdown();
    }
}

/// `PoolEvent` -> `EngineMsg` (plan §2/§3.4). Generation safety is already
/// enforced BEFORE this point: `nmp_transport::Pool`'s own translator drops
/// any frame/connect event tagged with a superseded generation before it
/// ever reaches this sink (see `nmp-transport::pool::inner`'s doc) — the
/// `TransportRelayHandle` carried inside `PoolEvent::Connected`/`Frame`
/// already embeds the (verified-current) generation, so forwarding it
/// unchanged into `EngineMsg::RelayConnected`/`RelayFrame` is exactly the
/// "tag frames with the handle's generation" step; there is no further
/// staleness check for this module to perform.
///
fn translate_pool_event(event: PoolEvent) -> Option<EngineMsg> {
    match event {
        PoolEvent::Connected { handle, session } => {
            Some(EngineMsg::RelayConnected(handle, session))
        }
        // The `reason` is no longer discarded here (issue #506's CRITICAL
        // fix): `EngineCore::on_relay_disconnected` needs to tell a
        // permanent failure (401/403 -- the relay worker has already
        // retired itself, see `nmp_transport::DisconnectReason::
        // PermanentlyFailed`'s doc) apart from an ordinary transient one, so
        // it never re-issues `Effect::EnsureRelay` into a busy 401 redial
        // loop.
        PoolEvent::Disconnected {
            handle,
            session,
            reason,
        } => Some(EngineMsg::RelayDisconnected(handle, session, reason)),
        PoolEvent::Frame {
            handle,
            session,
            frame,
        } => Some(EngineMsg::RelayFrame(handle, session, frame)),
        PoolEvent::Health {
            handle,
            session,
            health,
        } => Some(EngineMsg::RelayHealth(handle, session, health)),
        PoolEvent::EventHandoff {
            correlation,
            result,
        } => Some(EngineMsg::EventHandoff(correlation, result)),
        PoolEvent::WorkerRetired => None,
    }
}

/// Per-SESSION reconnect-preamble bookkeeping: the full set of currently-live
/// REQ wire texts, keyed by `SubId` so `WireOp::Req`/`Close` can update it
/// incrementally (module doc: `Pool::set_reconnect_preamble` replaces the
/// WHOLE preamble on every call, so this module must always hand it the
/// complete current set, not a delta). PROTECTED sessions never own an entry
/// here (#8): their REQs must never auto-replay on reconnect — a fresh
/// generation is unauthenticated until its own AUTH completes, and the
/// engine re-issues `Effect::Replay` itself on `RelayAuthReady`.
type Preambles = HashMap<RelaySessionKey, HashMap<SubId, String>>;

#[derive(Clone, Copy)]
struct DispatchRuntime<'a> {
    self_inbox: &'a Sender<Cmd>,
    relay_information: &'a RelayInformationService,
    native_tasks: &'a nmp_executor::Executor,
    nip11_decisions: &'a RefCell<Nip11DecisionState>,
}

#[derive(Default)]
struct Nip11DecisionState {
    next_generation: u64,
    pending: HashMap<RelayUrl, Nip11Decision>,
}

struct Nip11Decision {
    generation: u64,
    deadline: Instant,
    fallback_sent: bool,
}

impl Nip11DecisionState {
    fn begin(&mut self, url: RelayUrl, now: Instant) -> u64 {
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let generation = self.next_generation;
        self.pending.insert(
            url,
            Nip11Decision {
                generation,
                deadline: now + NIP11_DECISION_GRACE,
                fallback_sent: false,
            },
        );
        generation
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.pending
            .values()
            .filter(|decision| !decision.fallback_sent)
            .map(|decision| decision.deadline)
            .min()
    }

    fn take_due_fallbacks(&mut self, now: Instant) -> Vec<RelayUrl> {
        let mut due = Vec::new();
        for (url, decision) in &mut self.pending {
            if !decision.fallback_sent && decision.deadline <= now {
                decision.fallback_sent = true;
                due.push(url.clone());
            }
        }
        due
    }

    fn complete(&mut self, url: &RelayUrl, generation: u64) -> bool {
        if !self
            .pending
            .get(url)
            .is_some_and(|decision| decision.generation == generation)
        {
            return false;
        }
        self.pending.remove(url);
        true
    }

    fn refuse(&mut self, url: &RelayUrl, generation: u64) {
        if self
            .pending
            .get(url)
            .is_some_and(|decision| decision.generation == generation)
        {
            self.pending.remove(url);
        }
    }
}

#[cfg(test)]
mod nip11_decision_tests {
    use super::*;

    #[test]
    fn grace_fallback_is_independent_and_eventual_completion_is_generation_guarded() {
        let relay = RelayUrl::parse("wss://decision.example").unwrap();
        let now = Instant::now();
        let mut state = Nip11DecisionState::default();
        let generation = state.begin(relay.clone(), now);

        assert!(state
            .take_due_fallbacks(now + NIP11_DECISION_GRACE - Duration::from_millis(1))
            .is_empty());
        assert_eq!(
            state.take_due_fallbacks(now + NIP11_DECISION_GRACE),
            vec![relay.clone()]
        );
        assert!(state
            .take_due_fallbacks(now + NIP11_DECISION_GRACE + Duration::from_secs(1))
            .is_empty());
        assert!(!state.complete(&relay, generation.wrapping_add(1)));
        assert!(state.complete(&relay, generation));
        assert!(state.pending.is_empty());
    }
}

/// The engine thread's body: construct `EngineCore` (this is the ONLY place
/// it is ever built — it never leaves this stack frame), then block on
/// `cmd_rx` (D8) until `Cmd::Shutdown`.
///
/// The deadline-armed driver (§3.3, #39): every iteration re-reads
/// `core.next_deadline()` fresh, so a command that just introduced an
/// earlier deadline (e.g. ingesting an event that expires sooner than
/// whatever this loop was previously armed for) re-arms naturally on the
/// very next `recv`/`recv_timeout` call — the command itself is the wakeup,
/// with no separate "interrupt the sleep" machinery. `None` (no deadline
/// pending anywhere) blocks on plain `recv()`: a light embedder with no
/// expiring content and no open negentropy session pays nothing beyond the
/// ordinary command loop. `Some(deadline)` arms `recv_timeout` for exactly
/// the remaining wall-clock distance to it (zero if already past, e.g. a
/// persisted deadline that elapsed while the process was down — the very
/// first iteration catches that up through the identical `Tick` path). A
/// timeout dispatches `EngineMsg::Tick(wall_now())`, which runs the
/// mechanism `core::EngineCore::tick` already implements (NIP-40 expiry +
/// neg-liveness sweep -- unchanged by this driver), then `continue`s
/// straight back to the top so the timeout is recomputed from the deadline
/// set `tick` just changed, rather than re-arming from a stale value.
fn engine_loop<S, D>(
    store: S,
    directory: D,
    cap: usize,
    admission: RelayAdmissionPolicy,
    pool_runtime: EnginePoolRuntime,
    cmd_rx: &Receiver<Cmd>,
    self_inbox: &Sender<Cmd>,
) where
    S: EventStore,
    D: RelayDirectory + 'static,
{
    let EnginePoolRuntime {
        pool,
        stop: pool_stop_tx,
        native_tasks,
        relay_information,
    } = pool_runtime;
    let native_tasks = &native_tasks;
    let mut core = EngineCore::new(store, Box::new(directory), cap).with_relay_admission(admission);
    let mut row_channels: HashMap<HandleId, Sender<RowsMsg>> = HashMap::new();
    let mut history_channels: HashMap<HistorySessionId, LatestSender<HistoryMsg>> = HashMap::new();
    let mut diag_channels: HashMap<u64, LatestSender<DiagnosticsSnapshot>> = HashMap::new();
    let mut next_diag_id: u64 = 0;
    let mut preambles: Preambles = Preambles::new();
    let mut registry = SignerRegistry::default();
    let mut active_pubkey = None;
    let mut next_sign_event_id = 1u64;
    let mut sign_event_cancellations: HashMap<u64, Arc<SignEventTerminal>> = HashMap::new();
    let nip11_decisions = RefCell::new(Nip11DecisionState::default());
    let dispatch_runtime = DispatchRuntime {
        self_inbox,
        relay_information: &relay_information,
        native_tasks,
        nip11_decisions: &nip11_decisions,
    };

    // Recovery happens before the first externally-issued command. Pending
    // rows already live in the store; this only rebuilds ownership and may
    // replay exact durable attempt bytes whose Started fact was committed.
    let recovery_effects = core.recover_on_boot();
    dispatch_core_effects(
        &core,
        recovery_effects,
        &pool,
        &mut row_channels,
        &mut history_channels,
        &mut diag_channels,
        &mut preambles,
        &registry,
        dispatch_runtime,
    );

    loop {
        let core_wait = core
            .next_deadline()
            .map(|deadline| duration_until(deadline, Timestamp::now()));
        let nip11_wait = nip11_decisions
            .borrow()
            .next_deadline()
            .map(|deadline| deadline.saturating_duration_since(Instant::now()));
        let wait = match (core_wait, nip11_wait) {
            (Some(core), Some(nip11)) => Some(core.min(nip11)),
            (Some(wait), None) | (None, Some(wait)) => Some(wait),
            (None, None) => None,
        };
        let cmd = match wait {
            None => match cmd_rx.recv() {
                Ok(cmd) => cmd,
                Err(_) => break, // every `Sender` (incl. `self_inbox`) is gone.
            },
            Some(wait) => match cmd_rx.recv_timeout(wait) {
                Ok(cmd) => cmd,
                Err(RecvTimeoutError::Timeout) => {
                    // The engine's wall-clock deadlines and NIP-11's
                    // sub-second behavioral grace share this one event-driven
                    // wait. Fire only the sources actually due, then re-arm.
                    for url in nip11_decisions
                        .borrow_mut()
                        .take_due_fallbacks(Instant::now())
                    {
                        let effects = core.handle(EngineMsg::RelayInformationResolved(url, None));
                        dispatch_core_effects(
                            &core,
                            effects,
                            &pool,
                            &mut row_channels,
                            &mut history_channels,
                            &mut diag_channels,
                            &mut preambles,
                            &registry,
                            dispatch_runtime,
                        );
                    }
                    let wall_now = Timestamp::now();
                    if core
                        .next_deadline()
                        .is_some_and(|deadline| deadline <= wall_now)
                    {
                        let effects = core.handle(EngineMsg::Tick(wall_now));
                        dispatch_core_effects(
                            &core,
                            effects,
                            &pool,
                            &mut row_channels,
                            &mut history_channels,
                            &mut diag_channels,
                            &mut preambles,
                            &registry,
                            dispatch_runtime,
                        );
                    }
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            },
        };
        match cmd {
            Cmd::Shutdown => break,
            Cmd::RelayInformationFetched {
                url,
                generation,
                result,
            } => {
                if !nip11_decisions.borrow_mut().complete(&url, generation) {
                    continue;
                }
                let information = (*result)
                    .ok()
                    .map(|snapshot| snapshot.capability_evidence());
                let effects = core.handle(EngineMsg::RelayInformationResolved(url, information));
                dispatch_core_effects(
                    &core,
                    effects,
                    &pool,
                    &mut row_channels,
                    &mut history_channels,
                    &mut diag_channels,
                    &mut preambles,
                    &registry,
                    dispatch_runtime,
                );
            }
            Cmd::RelayBatch { frames, applied } => {
                let effects = core.handle(EngineMsg::RelayFrames(frames));
                dispatch_core_effects(
                    &core,
                    effects,
                    &pool,
                    &mut row_channels,
                    &mut history_channels,
                    &mut diag_channels,
                    &mut preambles,
                    &registry,
                    dispatch_runtime,
                );
                let _ = applied.send(());
            }
            Cmd::AddSigner { signer, reply } => {
                let result = registry.add(signer);
                let _ = reply.send(result.clone());
                if let Ok(registration) = result {
                    let effects = core.handle(EngineMsg::SignerAttached(registration.public_key()));
                    dispatch_core_effects(
                        &core,
                        effects,
                        &pool,
                        &mut row_channels,
                        &mut history_channels,
                        &mut diag_channels,
                        &mut preambles,
                        &registry,
                        dispatch_runtime,
                    );
                }
            }
            Cmd::RemoveSigner {
                registration,
                reply,
            } => {
                let _ = reply.send(registry.remove(&registration));
            }
            Cmd::SignEvent {
                unsigned,
                reservation,
                completion,
                reply,
            } => {
                let Some(author) = active_pubkey else {
                    let _ = reply.send(Err(SignEventError::NoActiveSigner));
                    continue;
                };
                if unsigned.pubkey != author {
                    let _ = reply.send(Err(SignEventError::InvalidRequest {
                        reason: "request author does not match the active account".to_string(),
                    }));
                    continue;
                }
                let expected_id = match validate_sign_request(&unsigned) {
                    Ok(expected_id) => expected_id,
                    Err(error) => {
                        let _ = reply.send(Err(error));
                        continue;
                    }
                };
                let Some(signer) = registry.signer_for(author) else {
                    let _ = reply.send(Err(SignEventError::NoActiveSigner));
                    continue;
                };

                let (terminal, cancelled) = SignEventTerminal::new();
                let shutdown_terminal = Arc::clone(&terminal);
                let starter = match reservation.start_with_cancel(move || {
                    shutdown_terminal.cancel();
                }) {
                    Ok(starter) => starter,
                    Err(error) => {
                        let error = match error {
                            nmp_executor::SpawnError::ThreadUnavailable { component, error } => {
                                SignEventError::ThreadUnavailable {
                                    component,
                                    reason: error.to_string(),
                                }
                            }
                            nmp_executor::SpawnError::ExecutorShutDown { .. } => {
                                SignEventError::EngineClosed
                            }
                        };
                        let _ = reply.send(Err(error));
                        continue;
                    }
                };

                let operation_id = next_sign_event_id;
                next_sign_event_id = next_sign_event_id.wrapping_add(1).max(1);
                let signer_result = match signer.sign(unsigned.clone()) {
                    SignerOp::Ready(result) => SignEventSignerResult::Ready(Box::new(result)),
                    SignerOp::Pending(pending) => SignEventSignerResult::Pending(pending),
                };

                sign_event_cancellations.insert(operation_id, Arc::clone(&terminal));
                if reply
                    .send(Ok(SignEventRegistration {
                        id: operation_id,
                        terminal: Arc::clone(&terminal),
                    }))
                    .is_err()
                {
                    sign_event_cancellations.remove(&operation_id);
                    terminal.cancel();
                    continue;
                }

                let inbox = self_inbox.clone();
                starter.run(move || {
                    let signer_result = match signer_result {
                        SignEventSignerResult::Ready(result) => Some(*result),
                        SignEventSignerResult::Pending(pending) => {
                            pending.recv_or_cancel(cancelled)
                        }
                    };
                    let result = match signer_result {
                        Some(result) if terminal.resolve() => {
                            result.map_err(signer_error).and_then(|signed| {
                                validate_signer_output(&unsigned, expected_id, signed)
                            })
                        }
                        Some(_) | None => Err(SignEventError::Cancelled),
                    };
                    let _ = inbox.send(Cmd::SignEventFinished(operation_id));
                    completion(result);
                });
            }
            Cmd::CancelSignEvent(id) => {
                if let Some(terminal) = sign_event_cancellations.remove(&id) {
                    terminal.cancel();
                }
            }
            Cmd::SignEventFinished(id) => {
                sign_event_cancellations.remove(&id);
            }
            Cmd::ObserveDiagnostics { reply } => {
                let id = next_diag_id;
                next_diag_id += 1;
                let (tx, rx) = latest_channel();
                // Same pool-count stitch as the `Effect::EmitDiagnostics` arm
                // (issue #121) — the proactive open-time snapshot must carry
                // the relay-cap rejection count too, not only the ones fanned
                // out later.
                let mut snapshot = core.diagnostics_snapshot();
                snapshot.sessions_rejected_over_cap = snapshot
                    .sessions_rejected_over_cap
                    .saturating_add(pool.admission_rejections());
                tx.send(snapshot);
                if reply.send((id, rx)).is_err() {
                    // Caller already gave up -- nothing to register.
                    continue;
                }
                diag_channels.insert(id, tx);
            }
            Cmd::UnobserveDiagnostics(id) => {
                diag_channels.remove(&id);
            }
            Cmd::ReattachReceipt { id, sink, reply } => {
                let found = core.reattach_receipt(id, sink);
                let _ = reply.send(found);
            }
            Cmd::PublishTracked {
                intent,
                sink,
                reply,
            } => {
                let mut effects = core.handle(EngineMsg::Tick(Timestamp::now()));
                let publish_effects = core.handle(EngineMsg::Publish(intent, sink));
                let result = publish_result(&publish_effects);
                let _ = reply.send(result);
                effects.extend(publish_effects);
                dispatch_core_effects(
                    &core,
                    effects,
                    &pool,
                    &mut row_channels,
                    &mut history_channels,
                    &mut diag_channels,
                    &mut preambles,
                    &registry,
                    dispatch_runtime,
                );
            }
            Cmd::Subscribe { query, reply } => {
                let effects = core.handle(EngineMsg::Subscribe(query, Box::new(NullRowSink)));
                // `on_subscribe` always emits exactly one `Effect::EmitRows`
                // for the handle it just created (its `last_evidence` starts
                // `None`, which can never equal `Some(_)` -- see
                // `core::mod`'s `refresh_handle`), so this is always found.
                let id = effects
                    .iter()
                    .find_map(|e| match e {
                        Effect::EmitRows(id, ..) if !row_channels.contains_key(id) => Some(*id),
                        _ => None,
                    })
                    .expect("Subscribe must yield a fresh EmitRows for its own handle");
                let (rows_tx, rows_rx) = mpsc::channel();
                row_channels.insert(id, rows_tx);
                if let Err(error) = preflight_query_relay_workers(&effects, &pool) {
                    row_channels.remove(&id);
                    let withdraw = core.handle(EngineMsg::Unsubscribe(id));
                    dispatch_core_effects(
                        &core,
                        withdraw,
                        &pool,
                        &mut row_channels,
                        &mut history_channels,
                        &mut diag_channels,
                        &mut preambles,
                        &registry,
                        dispatch_runtime,
                    );
                    let _ = reply.send(Err(error));
                    continue;
                }
                if reply.send(Ok((id, rows_rx))).is_err() {
                    // Caller already gave up on `subscribe()` -- withdraw
                    // immediately rather than leak a live demand atom nobody
                    // will ever read from.
                    row_channels.remove(&id);
                    let withdraw = core.handle(EngineMsg::Unsubscribe(id));
                    dispatch_core_effects(
                        &core,
                        withdraw,
                        &pool,
                        &mut row_channels,
                        &mut history_channels,
                        &mut diag_channels,
                        &mut preambles,
                        &registry,
                        dispatch_runtime,
                    );
                    continue;
                }
                dispatch_core_effects(
                    &core,
                    effects,
                    &pool,
                    &mut row_channels,
                    &mut history_channels,
                    &mut diag_channels,
                    &mut preambles,
                    &registry,
                    dispatch_runtime,
                );
            }
            Cmd::SubscribeHistory { query, reply } => {
                let effects = core.handle(EngineMsg::SubscribeHistory(
                    query,
                    Box::new(NullHistorySink),
                ));
                let Some(id) = effects.iter().find_map(|effect| match effect {
                    Effect::EmitHistory(id, _) if !history_channels.contains_key(id) => Some(*id),
                    _ => None,
                }) else {
                    let _ = reply.send(Err(EngineThreadError::ThreadUnavailable {
                        component: "history projection".to_string(),
                        reason: "history session could not open its canonical projection"
                            .to_string(),
                    }));
                    dispatch_core_effects(
                        &core,
                        effects,
                        &pool,
                        &mut row_channels,
                        &mut history_channels,
                        &mut diag_channels,
                        &mut preambles,
                        &registry,
                        dispatch_runtime,
                    );
                    continue;
                };
                let (history_tx, history_rx) = latest_channel();
                history_channels.insert(id, history_tx);
                if let Err(error) = preflight_query_relay_workers(&effects, &pool) {
                    history_channels.remove(&id);
                    let withdraw = core.handle(EngineMsg::UnsubscribeHistory(id));
                    dispatch_core_effects(
                        &core,
                        withdraw,
                        &pool,
                        &mut row_channels,
                        &mut history_channels,
                        &mut diag_channels,
                        &mut preambles,
                        &registry,
                        dispatch_runtime,
                    );
                    let _ = reply.send(Err(error));
                    continue;
                }
                if reply
                    .send(Ok((id, HistoryReceiver::new(history_rx))))
                    .is_err()
                {
                    history_channels.remove(&id);
                    let withdraw = core.handle(EngineMsg::UnsubscribeHistory(id));
                    dispatch_core_effects(
                        &core,
                        withdraw,
                        &pool,
                        &mut row_channels,
                        &mut history_channels,
                        &mut diag_channels,
                        &mut preambles,
                        &registry,
                        dispatch_runtime,
                    );
                    continue;
                }
                dispatch_core_effects(
                    &core,
                    effects,
                    &pool,
                    &mut row_channels,
                    &mut history_channels,
                    &mut diag_channels,
                    &mut preambles,
                    &registry,
                    dispatch_runtime,
                );
            }
            Cmd::RequestRows {
                id,
                at_least,
                reply,
            } => {
                let effects = core.handle(EngineMsg::RequestRows(id, at_least));
                let result = effects.iter().find_map(|effect| match effect {
                    Effect::HistoryLoadResult(session, result) if *session == id => {
                        Some(result.clone())
                    }
                    _ => None,
                });
                if result.as_ref().is_some_and(Result::is_ok) {
                    // Preflight the staged advance's (possibly empty) relay
                    // workers before it becomes observable.
                    if let Err(error) = preflight_query_relay_workers(&effects, &pool) {
                        let rollback = core.handle(EngineMsg::RollbackHistoryLoad(id));
                        dispatch_core_effects(
                            &core,
                            rollback,
                            &pool,
                            &mut row_channels,
                            &mut history_channels,
                            &mut diag_channels,
                            &mut preambles,
                            &registry,
                            dispatch_runtime,
                        );
                        let _ = reply.send(Err(HistoryAdvanceError::TransportUnavailable {
                            reason: error.to_string(),
                        }));
                        continue;
                    }
                    if reply.send(Ok(())).is_err() {
                        let rollback = core.handle(EngineMsg::RollbackHistoryLoad(id));
                        dispatch_core_effects(
                            &core,
                            rollback,
                            &pool,
                            &mut row_channels,
                            &mut history_channels,
                            &mut diag_channels,
                            &mut preambles,
                            &registry,
                            dispatch_runtime,
                        );
                        continue;
                    }
                    // Commit, then drive the post-commit continuation loop to
                    // convergence (#485): each commit may auto-stage the next
                    // advance (target still unmet, older boundary present,
                    // progress made). Bounded by `max_rows` — a non-progressing
                    // advance never re-stages.
                    let mut committed = core.handle(EngineMsg::CommitHistoryLoad(id));
                    loop {
                        let restaged = committed.iter().any(|effect| {
                            matches!(
                                effect,
                                Effect::HistoryLoadResult(session, Ok(())) if *session == id
                            )
                        });
                        if restaged && preflight_query_relay_workers(&committed, &pool).is_err() {
                            // The continuation advance's workers are
                            // unavailable. Frames already delivered stand; roll
                            // the staged continuation back and stop growing.
                            let rollback = core.handle(EngineMsg::RollbackHistoryLoad(id));
                            dispatch_core_effects(
                                &core,
                                committed,
                                &pool,
                                &mut row_channels,
                                &mut history_channels,
                                &mut diag_channels,
                                &mut preambles,
                                &registry,
                                dispatch_runtime,
                            );
                            dispatch_core_effects(
                                &core,
                                rollback,
                                &pool,
                                &mut row_channels,
                                &mut history_channels,
                                &mut diag_channels,
                                &mut preambles,
                                &registry,
                                dispatch_runtime,
                            );
                            break;
                        }
                        dispatch_core_effects(
                            &core,
                            committed,
                            &pool,
                            &mut row_channels,
                            &mut history_channels,
                            &mut diag_channels,
                            &mut preambles,
                            &registry,
                            dispatch_runtime,
                        );
                        if !restaged {
                            break;
                        }
                        committed = core.handle(EngineMsg::CommitHistoryLoad(id));
                    }
                    continue;
                } else {
                    let _ =
                        reply.send(result.unwrap_or(Err(HistoryAdvanceError::StoreUnavailable)));
                }
                dispatch_core_effects(
                    &core,
                    effects,
                    &pool,
                    &mut row_channels,
                    &mut history_channels,
                    &mut diag_channels,
                    &mut preambles,
                    &registry,
                    dispatch_runtime,
                );
            }
            Cmd::UnsubscribeHistory(id) => {
                history_channels.remove(&id);
                let effects = core.handle(EngineMsg::UnsubscribeHistory(id));
                dispatch_core_effects(
                    &core,
                    effects,
                    &pool,
                    &mut row_channels,
                    &mut history_channels,
                    &mut diag_channels,
                    &mut preambles,
                    &registry,
                    dispatch_runtime,
                );
            }
            Cmd::RelayWorkerRetired => {
                retry_required_relay_workers(&core, &pool, &mut preambles);
            }
            Cmd::Engine(EngineMsg::Unsubscribe(id)) => {
                let effects = core.handle(EngineMsg::Unsubscribe(id));
                // Drop the sender: the app's `Receiver` observes disconnect.
                row_channels.remove(&id);
                dispatch_core_effects(
                    &core,
                    effects,
                    &pool,
                    &mut row_channels,
                    &mut history_channels,
                    &mut diag_channels,
                    &mut preambles,
                    &registry,
                    dispatch_runtime,
                );
            }
            Cmd::Engine(EngineMsg::SetActivePubkey(pk)) => {
                // P3: active identity is a reactive read input. Accepted
                // writes separately pin their exact author at acceptance.
                let effects = core.handle(EngineMsg::SetActivePubkey(pk));
                active_pubkey = pk;
                dispatch_core_effects(
                    &core,
                    effects,
                    &pool,
                    &mut row_channels,
                    &mut history_channels,
                    &mut diag_channels,
                    &mut preambles,
                    &registry,
                    dispatch_runtime,
                );
            }
            Cmd::Engine(EngineMsg::Publish(intent, sink)) => {
                // Acceptance timestamps and NIP-40 refusal are wall-clock
                // facts. Advance the pure reducer clock immediately before
                // the one accept transaction; otherwise a fresh runtime's
                // clock would remain zero until its first unrelated deadline.
                let mut effects = core.handle(EngineMsg::Tick(Timestamp::now()));
                effects.extend(core.handle(EngineMsg::Publish(intent, sink)));
                dispatch_core_effects(
                    &core,
                    effects,
                    &pool,
                    &mut row_channels,
                    &mut history_channels,
                    &mut diag_channels,
                    &mut preambles,
                    &registry,
                    dispatch_runtime,
                );
            }
            Cmd::Engine(msg) => {
                let effects = core.handle(msg);
                dispatch_core_effects(
                    &core,
                    effects,
                    &pool,
                    &mut row_channels,
                    &mut history_channels,
                    &mut diag_channels,
                    &mut preambles,
                    &registry,
                    dispatch_runtime,
                );
            }
        }
    }

    for (_, terminal) in sign_event_cancellations.drain() {
        terminal.cancel();
    }

    // Tear down this thread's OWN `Pool` clone. If no other `Pool` clone
    // survives (the design here never keeps one anywhere else), this drops
    // the last `Arc<PoolInner>` reference after `shutdown` runs, which in
    // turn drops the pool's sink -- the very thing `EngineThread::join`'s
    // doc explains lets the bridge thread's `recv` finally disconnect.
    // Disconnecting the stop channel wakes the bridge if it is blocked on a
    // relay batch acknowledgement and wakes any bounded sink producer before
    // pool shutdown joins the translator.
    relay_information.close();
    drop(pool_stop_tx);
    pool.shutdown();
}

/// Release workers no longer owned by the reducer, then execute its effects.
/// Release MUST happen first: when a cap-sized plan replaces every relay,
/// keeping the old workers through `apply_wire_delta` would make every new
/// `ensure_open` fail even though the new plan itself is within the cap.
/// `required_relay_workers` includes nonterminal durable/ephemeral write work,
/// so this cannot evict a worker merely because its last read REQ vanished.
// Deliberately mirrors `dispatch_effects`' reviewed runtime destinations and
// adds only the reducer reference needed for exact ownership reconciliation.
#[allow(clippy::too_many_arguments)]
fn dispatch_core_effects<S: EventStore>(
    core: &EngineCore<S>,
    effects: Vec<Effect>,
    pool: &Pool,
    row_channels: &mut HashMap<HandleId, Sender<RowsMsg>>,
    history_channels: &mut HashMap<HistorySessionId, LatestSender<HistoryMsg>>,
    diag_channels: &mut HashMap<u64, LatestSender<DiagnosticsSnapshot>>,
    preambles: &mut Preambles,
    registry: &SignerRegistry,
    runtime: DispatchRuntime<'_>,
) {
    if let Some(required) = core.required_relay_workers() {
        for event in pool.close_unrequired_sessions(&required) {
            if let Some(msg) = translate_pool_event(event) {
                let _ = runtime.self_inbox.send(Cmd::Engine(msg));
            }
        }
        preambles.retain(|session, _| required.contains(session));
    }

    dispatch_effects(
        effects,
        pool,
        row_channels,
        history_channels,
        diag_channels,
        preambles,
        registry,
        runtime,
    );
}

/// Acquire the relay worker threads needed by one new query before its
/// synchronous handle crosses the supported facade. Capacity refusal remains
/// ordinary local shortfall, but an OS spawn refusal is returned as the typed
/// construction error #442 requires. Successful opens are idempotently reused
/// by ordinary effect dispatch.
fn preflight_query_relay_workers(effects: &[Effect], pool: &Pool) -> Result<(), EngineThreadError> {
    preflight_query_relay_workers_with(
        effects,
        |session| pool.live_session_handle(session).is_some(),
        |session| match pool.ensure_session(session) {
            Ok(handle) => Ok(Some(handle)),
            Err(nmp_transport::RelayOpenError::ThreadUnavailable(error)) => {
                Err(EngineThreadError::ThreadUnavailable {
                    component: error.role.to_string(),
                    reason: error.reason,
                })
            }
            // Capacity/unavailable remains ordinary local shortfall. It is
            // represented by acquisition evidence, not construction failure.
            Err(_) => Ok(None),
        },
        |handle| {
            let _ = pool.close(handle);
        },
    )
}

fn preflight_query_relay_workers_with(
    effects: &[Effect],
    mut is_live: impl FnMut(&RelaySessionKey) -> bool,
    mut ensure_session: impl FnMut(
        &RelaySessionKey,
    ) -> Result<Option<nmp_transport::RelayHandle>, EngineThreadError>,
    mut close: impl FnMut(nmp_transport::RelayHandle),
) -> Result<(), EngineThreadError> {
    let mut sessions = BTreeSet::new();
    for effect in effects {
        match effect {
            Effect::Wire(delta) => {
                for (session, ops) in &delta.ops {
                    if ops.iter().any(|op| matches!(op, WireOp::Req(..))) {
                        sessions.insert(session.clone());
                    }
                }
            }
            Effect::PreflightHistoryRelays(planned) => sessions.extend(planned.iter().cloned()),
            _ => {}
        }
    }

    let mut opened = Vec::new();
    for session in sessions {
        let was_live = is_live(&session);
        match ensure_session(&session) {
            Ok(Some(handle)) if !was_live => opened.push(handle),
            Ok(_) => {}
            Err(error) => {
                for handle in opened {
                    close(handle);
                }
                return Err(error);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod history_preflight_tests {
    use std::cell::RefCell;

    use nmp_grammar::{AccessContext, SourceAuthority};
    use nmp_transport::RelayHandle;

    use super::*;

    #[test]
    fn partial_history_preflight_failure_closes_every_worker_it_opened() {
        let first = RelayUrl::parse("wss://a-history-preflight.example").unwrap();
        let second = RelayUrl::parse("wss://b-history-preflight.example").unwrap();
        let filter = ConcreteFilter::default();
        let delta = WireDelta {
            ops: vec![
                (
                    RelaySessionKey::public(first.clone()),
                    vec![WireOp::Req(
                        SubId::for_wire(
                            first.clone(),
                            &filter,
                            &SourceAuthority::Public,
                            AccessContext::Public,
                        ),
                        filter.clone(),
                    )],
                ),
                (
                    RelaySessionKey::public(second.clone()),
                    vec![WireOp::Req(
                        SubId::for_wire(
                            second.clone(),
                            &filter,
                            &SourceAuthority::Public,
                            AccessContext::Public,
                        ),
                        filter,
                    )],
                ),
            ],
        };
        let effects = vec![Effect::Wire(delta)];
        let closed = RefCell::new(Vec::new());
        let result = preflight_query_relay_workers_with(
            &effects,
            |_| false,
            |session| {
                if session.relay == first {
                    Ok(Some(RelayHandle {
                        slot: 7,
                        generation: 1,
                    }))
                } else {
                    Err(EngineThreadError::ThreadUnavailable {
                        component: "relay worker".to_string(),
                        reason: "injected refusal".to_string(),
                    })
                }
            },
            |handle| closed.borrow_mut().push(handle),
        );

        assert!(matches!(
            result,
            Err(EngineThreadError::ThreadUnavailable { .. })
        ));
        assert_eq!(
            closed.into_inner(),
            vec![RelayHandle {
                slot: 7,
                generation: 1
            }]
        );
    }
}

/// Retry the exact currently-owned relay-session set once after an actual
/// worker join releases retirement capacity. Public read sessions replay the
/// full preamble retained even when their first spawn was refused;
/// write-only and PROTECTED sessions need only be opened, after which the
/// ordinary Connected (and, for protected, `RelayAuthReady`) path advances
/// them — a protected session's reconnect must never auto-send REQs (#8),
/// so its fresh worker gets an explicitly EMPTY reconnect preamble.
fn retry_required_relay_workers<S: EventStore>(
    core: &EngineCore<S>,
    pool: &Pool,
    preambles: &mut Preambles,
) {
    let Some(required) = core.required_relay_workers() else {
        return;
    };
    for session in required {
        if pool.live_session_handle(&session).is_some() {
            continue;
        }
        let Ok(handle) = pool.ensure_session(&session) else {
            continue;
        };
        if session.access != nmp_grammar::AccessContext::Public {
            pool.set_reconnect_preamble(handle, Vec::new());
            continue;
        }
        let Some(entry) = preambles.get(&session) else {
            continue;
        };
        let frames: Vec<_> = entry.values().cloned().collect();
        for frame in &frames {
            let _ = pool.send(handle, WireFrame::Text(frame.clone()));
        }
        pool.set_reconnect_preamble(handle, frames);
    }
}

/// Execute every `Effect` `EngineCore::handle` returned, in order.
// Deliberately spells out each reviewed runtime destination so effect routing
// cannot acquire hidden mutable state.
#[allow(clippy::too_many_arguments)]
fn dispatch_effects(
    effects: Vec<Effect>,
    pool: &Pool,
    row_channels: &mut HashMap<HandleId, Sender<RowsMsg>>,
    history_channels: &mut HashMap<HistorySessionId, LatestSender<HistoryMsg>>,
    diag_channels: &mut HashMap<u64, LatestSender<DiagnosticsSnapshot>>,
    preambles: &mut Preambles,
    registry: &SignerRegistry,
    runtime: DispatchRuntime<'_>,
) {
    for effect in effects {
        dispatch_effect(
            effect,
            pool,
            row_channels,
            history_channels,
            diag_channels,
            preambles,
            registry,
            runtime,
        );
    }
}

// Deliberately mirrors `dispatch_effects`; each destination remains explicit
// at the one-effect boundary where its ownership is audited.
#[allow(clippy::too_many_arguments)]
fn dispatch_effect(
    effect: Effect,
    pool: &Pool,
    row_channels: &mut HashMap<HandleId, Sender<RowsMsg>>,
    history_channels: &mut HashMap<HistorySessionId, LatestSender<HistoryMsg>>,
    diag_channels: &mut HashMap<u64, LatestSender<DiagnosticsSnapshot>>,
    preambles: &mut Preambles,
    registry: &SignerRegistry,
    runtime: DispatchRuntime<'_>,
) {
    match effect {
        Effect::Wire(delta) => apply_wire_delta(&delta, pool, preambles),
        Effect::PreflightHistoryRelays(_) => {}
        Effect::Replay(session, reqs) => apply_replay(&session, reqs, pool, preambles),
        Effect::FetchRelayInformation(url) => {
            let generation = runtime
                .nip11_decisions
                .borrow_mut()
                .begin(url.clone(), Instant::now());
            let inbox = runtime.self_inbox.clone();
            let callback_url = url.clone();
            let result = runtime.relay_information.request_callback(
                url.clone(),
                RelayInformationCachePolicy::UseCache,
                move |result| {
                    let _ = inbox.send(Cmd::RelayInformationFetched {
                        url: callback_url,
                        generation,
                        result: Box::new(result),
                    });
                },
            );
            if result.is_err() {
                runtime
                    .nip11_decisions
                    .borrow_mut()
                    .refuse(&url, generation);
                let _ = runtime
                    .self_inbox
                    .send(Cmd::Engine(EngineMsg::RelayInformationResolved(url, None)));
            }
        }
        Effect::PublishEvent(session, event, correlation) => {
            let Ok(handle) = pool.ensure_session(&session) else {
                let _ = runtime.self_inbox.send(Cmd::Engine(EngineMsg::EventHandoff(
                    correlation,
                    HandoffResult::NotHandedOff,
                )));
                return;
            };
            let json = ClientMessage::event(event).as_json();
            if let DurableSendOutcome::Resolved(result) =
                pool.send_durable(handle, correlation, WireFrame::Text(json))
            {
                let _ = runtime
                    .self_inbox
                    .send(Cmd::Engine(EngineMsg::EventHandoff(correlation, result)));
            }
        }
        Effect::EnsureRelay(session) => {
            // The durable lane is already persisted as WaitingConnection.
            // A typed cap refusal remains observable in pool diagnostics and
            // must not be converted back into an invalid handle or a busy
            // retry loop here.
            let _refusal = pool.ensure_session(&session).err();
        }
        // The signer frozen into this exact accepted template is looked up
        // by pubkey on every request. A later active-account switch cannot
        // redirect outstanding work. No matching registered signer is
        // NOT a terminal signer failure. The accepted pending row and
        // obligation stay alive as `AwaitingCapability`; only an explicit
        // denial/error from an attached signer compensates the write.
        Effect::RequestSign(id, generation, unsigned) => match registry.signer_for(unsigned.pubkey)
        {
            Some(signer) => match signer.sign(unsigned) {
                SignerOp::Ready(result) => {
                    let _ = runtime
                        .self_inbox
                        .send(Cmd::Engine(EngineMsg::SignerCompleted(
                            id, generation, result,
                        )));
                }
                SignerOp::Pending(pending) => {
                    // A single blocking recv on a fresh thread, then exactly
                    // one forwarded message -- D8-compliant (no poll loop),
                    // and keeps the engine thread itself from ever blocking
                    // on a remote signer round-trip.
                    let inbox = runtime.self_inbox.clone();
                    let (cancel, cancelled) = pending_signer_cancellation();
                    let result = runtime.native_tasks.spawn_with_cancel(
                        "engine-signer-waiter",
                        move || cancel.cancel(),
                        move || {
                            let result = pending
                                .recv_or_cancel(cancelled)
                                .unwrap_or(Err(nmp_signer::SignerError::Disconnected));
                            let _ = inbox.send(Cmd::Engine(EngineMsg::SignerCompleted(
                                id, generation, result,
                            )));
                        },
                    );
                    if result.is_err() {
                        let _ = runtime
                            .self_inbox
                            .send(Cmd::Engine(EngineMsg::SignerCompleted(
                                id,
                                generation,
                                Err(nmp_signer::SignerError::Unavailable),
                            )));
                    }
                }
            },
            None => {
                let _ = runtime
                    .self_inbox
                    .send(Cmd::Engine(EngineMsg::SignerUnavailable(id, generation)));
            }
        },
        Effect::RearmSignerIfAvailable(pubkey) => {
            if registry
                .signer_for(pubkey)
                .is_some_and(SigningCapability::is_available)
            {
                let _ = runtime
                    .self_inbox
                    .send(Cmd::Engine(EngineMsg::SignerAttached(pubkey)));
            }
        }
        Effect::RequestDecrypt(..) => {
            // No `EngineMsg` feedback path exists yet to carry a decrypted
            // result back into `EngineCore` (B never wired one -- see the
            // plan's §8 underspecified item 2, "confirm before E"). Adding
            // one is out of this builder's scope (core wiring is limited to
            // what frame parsing needs); left as an explicit no-op, the same
            // as E's `StartProbe`/`NegOpen` stubs below.
        }
        Effect::RecordCoverage(..) => {
            // `EngineCore::on_relay_frame`'s EOSE arm already calls
            // `EventStore::record_coverage` itself before ever returning
            // this effect (see `core/mod.rs`) -- this is a notification for
            // an observer, not a command this runtime must additionally act
            // on.
        }
        Effect::EmitRows(id, rows, evidence) => {
            if let Some(tx) = row_channels.get(&id) {
                let _ = tx.send((rows, evidence));
            }
        }
        Effect::EmitHistory(id, batch) => {
            if let Some(tx) = history_channels.get(&id) {
                tx.send(batch);
            }
        }
        Effect::HistoryLoadResult(..) => {}
        Effect::EmitDiagnostics(mut snapshot) => {
            // Fold in the transport pool's own relay-cap rejection count
            // (issue #121, worker-exhaustion half). `EngineCore` builds the
            // snapshot with this field `0` because it has no view of the
            // pool's slot table; the runtime edge is the one place that holds
            // both the core-built snapshot AND the `Pool`, so it stitches the
            // count in here before fan-out. Idempotent per snapshot (a fresh
            // read each time), monotonic across snapshots.
            snapshot.sessions_rejected_over_cap = snapshot
                .sessions_rejected_over_cap
                .saturating_add(pool.admission_rejections());
            // Fan out to every currently-registered observer (M5 plan §1.2
            // step 4) -- each observer's own `LatestSender` overwrites its
            // own slot, so a slow consumer only ever sees the newest
            // snapshot next (see `diagnostics_channel`'s doc), never a
            // growing backlog.
            for tx in diag_channels.values() {
                tx.send(snapshot.clone());
            }
        }
        Effect::EmitReceipt(..) => {
            // The `ReceiptSink` passed to `Publish` already delivered this
            // exact `WriteStatus` synchronously inside `EngineCore` (see the
            // module doc's "two delivery channels" note) -- redelivering
            // here would just duplicate it.
        }
        Effect::PublishFailed(..) => {
            // `PublishTracked` consumes this typed pre-receipt failure for
            // its synchronous reply. There is no receipt stream to fan out.
        }
        Effect::StartProbe(url, sub_id, filter, initial_hex) => {
            let Ok(handle) = pool.ensure_open(&url) else {
                return;
            };
            let text = neg_open_frame_text(&sub_id, &filter, initial_hex);
            let _ = pool.send(handle, WireFrame::Text(text));
        }
        Effect::NegOpen(probed, sub_id, filter, initial_hex) => {
            let relay = probed.url().clone();
            let Ok(handle) = pool.ensure_open(&relay) else {
                return;
            };
            let text = neg_open_frame_text(&sub_id, &filter, initial_hex);
            let _ = pool.send(handle, WireFrame::Text(text));
        }
        Effect::NegMsg(relay, sub_id, message_hex) => {
            let Ok(handle) = pool.ensure_open(&relay) else {
                return;
            };
            let text = neg_msg_frame_text(&sub_id, message_hex);
            let _ = pool.send(handle, WireFrame::Text(text));
        }
        Effect::NegClose(relay, sub_id) => {
            let Ok(handle) = pool.ensure_open(&relay) else {
                return;
            };
            let text = neg_close_frame_text(&sub_id);
            let _ = pool.send(handle, WireFrame::Text(text));
        }
    }
}

/// The wire `["NEG-OPEN", sub_id, filter, initial_message]` text for
/// `sub_id`/`filter` -- the SAME wire subscription-id convention
/// `req_frame_text`/`close_frame_text` use (`core::wire_sub_id_string`),
/// since REQ and NEG-OPEN share one subscription-id namespace on the wire
/// (NIP-77) and `core::mod`'s attribution/session bookkeeping looks either
/// up by that identical literal string.
fn neg_open_frame_text(
    sub_id: &SubId,
    filter: &ConcreteFilter,
    initial_message_hex: String,
) -> String {
    let wire_id = SubscriptionId::new(core::wire_sub_id_string(sub_id));
    ClientMessage::neg_open(wire_id, filter.to_nostr(), initial_message_hex).as_json()
}

/// The wire `["NEG-MSG", sub_id, message]` text for `sub_id` -- `nostr`
/// 0.44.4 exposes no `ClientMessage::neg_msg` constructor helper (only
/// `neg_open`/`req`/`close`/etc.), so the variant is built directly; its
/// fields are public on the public `ClientMessage` enum.
fn neg_msg_frame_text(sub_id: &SubId, message_hex: String) -> String {
    let wire_id = SubscriptionId::new(core::wire_sub_id_string(sub_id));
    ClientMessage::NegMsg {
        subscription_id: std::borrow::Cow::Owned(wire_id),
        message: std::borrow::Cow::Owned(message_hex),
    }
    .as_json()
}

/// The wire `["NEG-CLOSE", sub_id]` text for `sub_id` (same wire-id
/// convention as [`neg_open_frame_text`]/[`neg_msg_frame_text`]).
fn neg_close_frame_text(sub_id: &SubId) -> String {
    let wire_id = SubscriptionId::new(core::wire_sub_id_string(sub_id));
    ClientMessage::NegClose {
        subscription_id: std::borrow::Cow::Owned(wire_id),
    }
    .as_json()
}

/// `Effect::Wire`'s per-session ops -> wire frames + reconnect-preamble
/// upkeep. `ensure_session` is idempotent for an already-live slot (ships
/// the frame onto whichever generation is current, queuing it if the socket
/// is still dialing) and transparently reopens a previously-closed one, so
/// there is no separate "is this session already open" bookkeeping to keep
/// here.
///
/// PROTECTED sessions take a stricter path (#8): their frames are sent
/// directly (the reducer only ever emits protected ops AFTER the exact
/// current generation reported `RelayAuthReady`), but NO reconnect preamble
/// is ever stored for them — a fresh generation is unauthenticated until
/// its own AUTH completes, so the pool must never auto-replay a protected
/// REQ, and this module keeps no `preambles` entry that
/// `retry_required_relay_workers` could accidentally resend.
fn apply_wire_delta(delta: &WireDelta, pool: &Pool, preambles: &mut Preambles) {
    for (session, ops) in &delta.ops {
        let has_req = ops.iter().any(|op| matches!(op, WireOp::Req(..)));
        let handle = if has_req {
            pool.ensure_session(session).ok()
        } else {
            // A close-only delta must never reopen a worker already released
            // by exact session-demand reconciliation. Socket teardown already
            // withdrew every subscription on that connection.
            pool.live_session_handle(session)
        };
        if session.access != nmp_grammar::AccessContext::Public {
            for op in ops {
                let text = match op {
                    WireOp::Req(sub_id, filter) => req_frame_text(sub_id, filter),
                    WireOp::Close(sub_id) => close_frame_text(sub_id),
                };
                if let Some(handle) = handle {
                    let _ = pool.send(handle, WireFrame::Text(text));
                }
            }
            if let Some(handle) = handle {
                pool.set_reconnect_preamble(handle, Vec::new());
            }
            preambles.remove(session);
            continue;
        }
        let entry = preambles.entry(session.clone()).or_default();
        for op in ops {
            match op {
                WireOp::Req(sub_id, filter) => {
                    let text = req_frame_text(sub_id, filter);
                    if let Some(handle) = handle {
                        let _ = pool.send(handle, WireFrame::Text(text.clone()));
                    }
                    entry.insert(sub_id.clone(), text);
                }
                WireOp::Close(sub_id) => {
                    let text = close_frame_text(sub_id);
                    if let Some(handle) = handle {
                        let _ = pool.send(handle, WireFrame::Text(text));
                    }
                    entry.remove(sub_id);
                }
            }
        }
        let frames: Vec<String> = entry.values().cloned().collect();
        let empty = frames.is_empty();
        if let Some(handle) = handle {
            pool.set_reconnect_preamble(handle, frames);
        }
        if empty {
            preambles.remove(session);
        }
    }
}

/// `Effect::Replay`: `reqs` is `EngineCore`'s full CURRENT req list for
/// `session` at the moment it observed `RelayConnected` (`core/mod.rs`'s
/// `on_relay_connected`) or — for a protected session — `RelayAuthReady`
/// (`on_relay_auth_ready`) -- an authoritative snapshot, not a delta, so the
/// preamble entry for this session is rebuilt from scratch rather than
/// patched. Resending these as fresh REQ frames on the just-connected handle
/// is what makes reconnection replay observable even on the very first
/// `Connected` for a session (before any preamble could have existed yet);
/// on a later automatic reconnect the pool's own preamble mechanism will
/// typically have already replayed them, and resending here is a harmless,
/// idempotent overwrite (NIP-01: a REQ with an existing sub-id replaces that
/// sub). A PROTECTED session's replay sends directly and stores NO preamble
/// (#8) — the same never-auto-replay rule as `apply_wire_delta`.
fn apply_replay(
    session: &RelaySessionKey,
    reqs: Vec<WireReq>,
    pool: &Pool,
    preambles: &mut Preambles,
) {
    let Ok(handle) = pool.ensure_session(session) else {
        return;
    };
    if session.access != nmp_grammar::AccessContext::Public {
        for req in &reqs {
            let text = req_frame_text(&req.sub_id, &req.filter);
            let _ = pool.send(handle, WireFrame::Text(text));
        }
        pool.set_reconnect_preamble(handle, Vec::new());
        preambles.remove(session);
        return;
    }
    let entry = preambles.entry(session.clone()).or_default();
    entry.clear();
    for req in &reqs {
        let text = req_frame_text(&req.sub_id, &req.filter);
        let _ = pool.send(handle, WireFrame::Text(text.clone()));
        entry.insert(req.sub_id.clone(), text);
    }
    let frames: Vec<String> = entry.values().cloned().collect();
    pool.set_reconnect_preamble(handle, frames);
}

/// The wire `["REQ", sub_id, filter]` text for `sub_id`/`filter`, using the
/// EXACT same wire subscription-id string `core::attribution` records at
/// send time (`core::wire_sub_id_string`) -- the relay echoes this string
/// back verbatim in EOSE/CLOSED, and `AttributionState::attribute_eose`
/// looks it up by that literal string, so any divergence here would silently
/// break coverage attribution.
fn req_frame_text(sub_id: &SubId, filter: &ConcreteFilter) -> String {
    let wire_id = SubscriptionId::new(core::wire_sub_id_string(sub_id));
    ClientMessage::req(wire_id, vec![filter.to_nostr()]).as_json()
}

/// The wire `["CLOSE", sub_id]` text for `sub_id` (same wire-id convention
/// as [`req_frame_text`]).
fn close_frame_text(sub_id: &SubId) -> String {
    let wire_id = SubscriptionId::new(core::wire_sub_id_string(sub_id));
    ClientMessage::close(wire_id).as_json()
}

/// The app-facing handle to a live diagnostics stream (returned by
/// [`Handle::observe_diagnostics`]). Withdraw it via [`Self::cancel`] when
/// the caller is done; unlike [`QueryHandle`] there is no `Drop` teardown
/// HERE (this value carries no resource of its own beyond the registry
/// entry it names) — `nmp-ffi`'s `NmpDiagnosticsHandle` is what ties
/// teardown to `Drop`, mirroring `NmpQueryHandle`'s own wrapper.
#[derive(Clone)]
pub struct DiagnosticsHandle {
    inbox: Sender<Cmd>,
    id: u64,
}

impl DiagnosticsHandle {
    /// Withdraw this diagnostics observer. Safe to call more than once
    /// (`Cmd::UnobserveDiagnostics` on an already-removed id is a harmless
    /// no-op); safe to never call at all (the registry entry simply
    /// outlives the caller's interest — a stream nobody drains yet, mirrors
    /// an app that never calls a `QueryHandle`'s `cancel`).
    pub fn cancel(&self) {
        let _ = self.inbox.send(Cmd::UnobserveDiagnostics(self.id));
    }
}

/// The cheap, `Clone + Send` app-facing handle. Its deliberately narrow
/// vocabulary preserves ledger #2/#3 at the top edge. M4 §5 added signer
/// registration to close the multi-account gap; M5 added read-only
/// diagnostics; #464 adds governed sign-only without creating a third
/// workload noun or bypassing the active-signer boundary:
///
/// - `subscribe(LiveQuery) -> (QueryHandle, Receiver<RowsMsg>)`
/// - `unsubscribe(QueryHandle)`
/// - `add_signer(impl SigningCapability) -> Result<SignerRegistration, AddSignerError>`
/// - `remove_signer(SignerRegistration) -> bool`
/// - `sign_event(UnsignedEvent) -> SignEventOperation`
/// - `set_active_account(Option<PublicKey>)`
/// - `publish(WriteIntent) -> Receiver<WriteStatus>`
/// - `observe_diagnostics() -> (DiagnosticsHandle, LatestReceiver<DiagnosticsSnapshot>)`
/// - `shutdown()`
///
/// No `relays:` parameter, no open-REQ method — internally every verb just
/// sends a [`Cmd`] onto the owning [`EngineThread`]'s inbox.
#[derive(Clone)]
pub struct Handle {
    inbox: Sender<Cmd>,
    native_tasks: nmp_executor::Executor,
    relay_information: RelayInformationService,
}

/// One accepted sign-only operation. It owns no write receipt or durable
/// obligation: dropping it before completion cancels the exact signer RPC.
pub struct SignEventOperation {
    result: Option<Receiver<Result<SignedEvent, SignEventError>>>,
    cancel: SignEventCancel,
}

enum SignEventSignerResult {
    Ready(Box<Result<SignedEvent, nmp_signer::SignerError>>),
    Pending(PendingSignerOp<SignedEvent>),
}

impl SignEventOperation {
    pub fn recv(mut self) -> Result<SignedEvent, SignEventError> {
        self.result
            .take()
            .expect("sign-event result is consumed exactly once")
            .recv()
            .unwrap_or(Err(SignEventError::Cancelled))
    }

    #[must_use]
    pub fn cancel_handle(&self) -> SignEventCancel {
        self.cancel.clone()
    }
}

impl Drop for SignEventOperation {
    fn drop(&mut self) {
        if self.result.is_some() {
            self.cancel.cancel();
        }
    }
}

/// Idempotent cancellation token for one exact sign-only operation.
#[derive(Clone)]
pub struct SignEventCancel {
    inbox: Sender<Cmd>,
    id: u64,
    terminal: Arc<SignEventTerminal>,
}

impl SignEventCancel {
    pub fn cancel(&self) {
        if self.terminal.cancel() {
            let _ = self.inbox.send(Cmd::CancelSignEvent(self.id));
        }
    }
}

/// Opaque ownership proof for one exact signer-registry installation.
/// Replacing a signer for the same public key creates a distinct value, so
/// cleanup from the older provider cannot detach the replacement.
#[derive(Clone)]
pub struct SignerRegistration {
    public_key: PublicKey,
    identity: Arc<()>,
}

impl SignerRegistration {
    #[must_use]
    pub fn public_key(&self) -> PublicKey {
        self.public_key
    }
}

impl std::fmt::Debug for SignerRegistration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignerRegistration")
            .field("public_key", &self.public_key)
            .finish_non_exhaustive()
    }
}

impl PartialEq for SignerRegistration {
    fn eq(&self, other: &Self) -> bool {
        self.public_key == other.public_key && Arc::ptr_eq(&self.identity, &other.identity)
    }
}

impl Eq for SignerRegistration {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddSignerError {
    MissingPublicKey,
}

impl std::fmt::Display for AddSignerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingPublicKey => f.write_str("signing capability has no public key"),
        }
    }
}

impl std::error::Error for AddSignerError {}

/// Test-only proof seam for hidden NIP-11 cache/flight ownership. It is a
/// free function specifically so the reviewed [`Handle`] verb set cannot grow
/// an alternate command surface.
#[cfg(feature = "test-instrumentation")]
#[doc(hidden)]
pub fn relay_information_retention_census(
    handle: &Handle,
) -> crate::relay_information::RelayInformationRetentionCensus {
    handle.relay_information.retention_census()
}

impl Handle {
    /// Acquire NIP-11 once through the engine-owned cache. This may block
    /// the CALLER on HTTP, never the reducer thread. The resolved
    /// advertisement is also fed back into capability decision-making.
    pub fn relay_information(
        &self,
        relay: RelayUrl,
        policy: RelayInformationCachePolicy,
    ) -> Result<RelayInformationSnapshot, RelayInformationError> {
        let snapshot = self.relay_information.get(relay.clone(), policy)?;
        let information = snapshot.capability_evidence();
        let _ = self
            .inbox
            .send(Cmd::Engine(EngineMsg::RelayInformationResolved(
                relay,
                Some(information),
            )));
        Ok(snapshot)
    }

    /// Async form for public/FFI consumers. HTTP remains on the bounded
    /// engine-owned workers; awaiting this never blocks a native UI thread.
    pub async fn relay_information_async(
        &self,
        relay: RelayUrl,
        policy: RelayInformationCachePolicy,
    ) -> Result<RelayInformationSnapshot, RelayInformationError> {
        let snapshot = self
            .relay_information
            .get_async(relay.clone(), policy)
            .await?;
        let information = snapshot.capability_evidence();
        let _ = self
            .inbox
            .send(Cmd::Engine(EngineMsg::RelayInformationResolved(
                relay,
                Some(information),
            )));
        Ok(snapshot)
    }

    /// Open a live subscription. Blocks (briefly — one engine-thread round
    /// trip, never network-bound) until `EngineCore` has assigned the
    /// `HandleId` and the row channel is registered, then returns both. An
    /// OS refusal to create an initially required relay worker rolls the
    /// subscription back and returns [`EngineThreadError::ThreadUnavailable`]
    /// before a handle escapes.
    ///
    /// # Panics
    /// If the engine thread has already shut down. Calling `subscribe`
    /// after `shutdown` is a caller bug, not a recoverable runtime state —
    /// there is no engine left to own the subscription.
    pub fn subscribe(
        &self,
        query: LiveQuery,
    ) -> Result<(QueryHandle, Receiver<RowsMsg>), EngineThreadError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::Subscribe {
                query,
                reply: reply_tx,
            })
            .expect("nmp-engine: subscribe() called after the engine thread shut down");
        let (id, rows_rx) = reply_rx
            .recv()
            .expect("nmp-engine: engine thread dropped the subscribe reply")?;
        Ok((QueryHandle(id), rows_rx))
    }

    /// Withdraw a live subscription. Fire-and-forget: once the engine thread
    /// processes it, the row channel's `Sender` is dropped and the app's
    /// `Receiver` observes a clean disconnect.
    pub fn unsubscribe(&self, handle: QueryHandle) {
        let _ = self
            .inbox
            .send(Cmd::Engine(EngineMsg::Unsubscribe(handle.0)));
    }

    pub fn subscribe_history(
        &self,
        query: HistoryQuery,
    ) -> Result<(HistoryHandle, HistoryReceiver), EngineThreadError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::SubscribeHistory {
                query,
                reply: reply_tx,
            })
            .expect("nmp-engine: subscribe_history() called after shutdown");
        let (id, history_rx) = reply_rx
            .recv()
            .expect("nmp-engine: engine dropped the history subscribe reply")?;
        Ok((HistoryHandle(id), history_rx))
    }

    /// Declaratively raise a window's row target to at least `at_least`
    /// (#485). Monotonic, idempotent, and clamped to the window's declared
    /// `max_rows`. Returns `None` when the engine thread is gone (the facade
    /// maps this to `EngineClosed`); `Some(Ok(()))` when the advance was
    /// accepted (or was a no-op / `AtBound` beat); `Some(Err(_))` for a staged
    /// advance the store or transport could not serve.
    pub fn request_rows(
        &self,
        handle: HistoryHandle,
        at_least: usize,
    ) -> Option<Result<(), HistoryAdvanceError>> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::RequestRows {
                id: handle.0,
                at_least,
                reply: reply_tx,
            })
            .ok()?;
        reply_rx.recv().ok()
    }

    pub fn unsubscribe_history(&self, handle: HistoryHandle) {
        let _ = self.inbox.send(Cmd::UnsubscribeHistory(handle.0));
    }

    /// Register a signing/crypto capability, keyed by its own `public_key()`
    /// (M4 §5: `SignerRegistry`). Registering a signer does NOT make it
    /// active — call [`Self::set_active_account`] to actually switch reads
    /// and writes onto it. Blocks briefly (one engine-thread round trip,
    /// same discipline as [`Self::subscribe`]) and returns an opaque scoped
    /// registration. The registration exposes the key and is the only value
    /// that may later detach this exact installation.
    ///
    /// # Panics
    /// If the engine thread has already shut down.
    pub fn add_signer<Sig>(&self, signer: Sig) -> Result<SignerRegistration, AddSignerError>
    where
        Sig: SigningCapability + Send + 'static,
    {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::AddSigner {
                signer: Box::new(signer),
                reply: reply_tx,
            })
            .expect("nmp-engine: add_signer() called after the engine thread shut down");
        reply_rx
            .recv()
            .expect("nmp-engine: engine thread dropped the add_signer reply")
    }

    /// Detach this exact signer installation if it is still current.
    /// Accepted writes keep their frozen identity and remain waiting; they
    /// are never retargeted. A stale registration returns `false` and cannot
    /// remove a newer provider for the same public key.
    pub fn remove_signer(&self, registration: SignerRegistration) -> bool {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::RemoveSigner {
                registration,
                reply: reply_tx,
            })
            .expect("nmp-engine: remove_signer() called after shutdown");
        reply_rx
            .recv()
            .expect("nmp-engine: engine thread dropped the remove_signer reply")
    }

    /// Ask the currently active registered signer to sign one exact event,
    /// without accepting a write or touching the canonical store/outbox.
    /// Admission reserves a finite native-task slot before the signer is
    /// invoked; a pending remote operation is cancellable through the
    /// returned handle and engine shutdown.
    pub fn sign_event(
        &self,
        unsigned: UnsignedEvent,
    ) -> Result<SignEventOperation, SignEventError> {
        let (completion_tx, completion_rx) = mpsc::channel();
        let cancel = self.sign_event_with_completion(unsigned, move |result| {
            let _ = completion_tx.send(result);
        })?;
        Ok(SignEventOperation {
            result: Some(completion_rx),
            cancel,
        })
    }

    #[doc(hidden)]
    pub fn sign_event_with_completion(
        &self,
        unsigned: UnsignedEvent,
        completion: impl FnOnce(Result<SignedEvent, SignEventError>) + Send + 'static,
    ) -> Result<SignEventCancel, SignEventError> {
        let reservation = self.native_tasks.reserve("sign-event").map_err(|error| {
            SignEventError::ExecutorSaturated {
                capacity: error.capacity,
            }
        })?;
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::SignEvent {
                unsigned,
                reservation,
                completion: Box::new(completion),
                reply: reply_tx,
            })
            .map_err(|_| SignEventError::EngineClosed)?;
        let registration = reply_rx
            .recv()
            .map_err(|_| SignEventError::EngineClosed)??;
        Ok(SignEventCancel {
            inbox: self.inbox.clone(),
            id: registration.id,
            terminal: registration.terminal,
        })
    }

    /// Re-root every reactive query and default unsigned-publish authority
    /// onto `pk` (or onto none). Accepted writes are not redirected: each
    /// resolves the signer identity frozen at its acceptance boundary.
    /// `pk` need not already be registered via [`Self::add_signer`] — e.g.
    /// read-only browsing of an account this app holds no key for is legal. Publishing
    /// resolves the signer pinned by the draft's own author; if none is
    /// registered, the accepted intent remains `AwaitingCapability`.
    pub fn set_active_account(&self, pk: Option<PublicKey>) {
        let _ = self.inbox.send(Cmd::Engine(EngineMsg::SetActivePubkey(pk)));
    }

    /// Enqueue a write. Fire-and-forget: the returned `Receiver` streams
    /// every `WriteStatus` this intent ever reaches (ledger #9 — enqueue is
    /// not converged; the FIRST value is never a terminal for a durable/
    /// at-most-once intent. `Ephemeral` also yields receipt facts, but owns
    /// no durable delivery obligation or query-visible pending row. If no
    /// pre-acceptance correlation id remains, this returns a typed error and
    /// creates no receipt stream.
    pub fn publish(&self, intent: WriteIntent) -> Result<Receiver<WriteStatus>, PublishError> {
        self.publish_tracked(intent).map(|receipt| receipt.statuses)
    }

    /// Enqueue a write and expose its stable receipt id. This synchronous
    /// round trip waits only for the local crash-atomic acceptance door,
    /// never for signing, routing, network I/O, or ACKs. Correlation-id
    /// exhaustion is returned before any stream or identity is fabricated.
    pub fn publish_tracked(&self, intent: WriteIntent) -> Result<ReceiptStream, PublishError> {
        let (tx, rx) = mpsc::channel();
        let sink: Box<dyn ReceiptSink> = Box::new(ChannelReceiptSink(tx));
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::PublishTracked {
                intent,
                sink,
                reply: reply_tx,
            })
            .expect("nmp-engine: publish called after shutdown");
        let id = reply_rx
            .recv()
            .expect("nmp-engine: engine dropped publish receipt reply")?;
        Ok(ReceiptStream { id, statuses: rx })
    }

    /// Attach an additional observer to a retained receipt. The returned
    /// channel is primed with durable receipt/attempt facts. Missing and
    /// retained-but-unreadable evidence are distinct outcomes.
    pub fn reattach_receipt(&self, id: ReceiptId) -> ReceiptReattachment {
        let (tx, rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::ReattachReceipt {
                id,
                sink: Box::new(ChannelReceiptSink(tx)),
                reply: reply_tx,
            })
            .expect("nmp-engine: reattach called after shutdown");
        match reply_rx
            .recv()
            .expect("nmp-engine: engine dropped reattach reply")
        {
            ReattachOutcome::Attached => ReceiptReattachment::Attached(rx),
            ReattachOutcome::NotFound => ReceiptReattachment::NotFound,
            ReattachOutcome::RetainedButUnreadable => ReceiptReattachment::RetainedButUnreadable,
        }
    }

    /// Open a live diagnostics stream (M5 plan §1.2 step 4) — see
    /// `EngineCore::diagnostics_snapshot`'s doc for what it contains: this is
    /// the read-only projection combining per-relay wire-sub count, exact
    /// filters, lane counts, reverse coverage, events-received-per-kind, and
    /// per-filter coverage, engine-global (one stream, not per-query).
    /// Delivers the CURRENT snapshot immediately, then a fresh one on every
    /// recompile and every EOSE-driven coverage change — pushed reactively,
    /// never polled (D8); latest-wins if the consumer is slow (see
    /// `diagnostics_channel`'s doc — no unbounded backlog, no dropped
    /// row-equivalent data since this is a recomputed projection, not a
    /// delta stream). Blocks briefly (one engine-thread round trip, same
    /// discipline as [`Self::subscribe`]/[`Self::add_signer`]).
    ///
    /// # Panics
    /// If the engine thread has already shut down.
    #[must_use]
    pub fn observe_diagnostics(&self) -> (DiagnosticsHandle, LatestReceiver<DiagnosticsSnapshot>) {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::ObserveDiagnostics { reply: reply_tx })
            .expect("nmp-engine: observe_diagnostics() called after the engine thread shut down");
        let (id, rx) = reply_rx
            .recv()
            .expect("nmp-engine: engine thread dropped the observe_diagnostics reply");
        (
            DiagnosticsHandle {
                inbox: self.inbox.clone(),
                id,
            },
            rx,
        )
    }

    /// Stop the engine thread (and, transitively, the pool-bridge thread —
    /// see [`EngineThread::join`]). Idempotent: a `Handle` clone calling this
    /// after another already has just finds the inbox gone and no-ops.
    pub fn shutdown(&self) {
        let _ = self.inbox.send(Cmd::Shutdown);
    }
}
