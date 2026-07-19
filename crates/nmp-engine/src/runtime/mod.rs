//! The async edge (plan §2 position 2). `EngineThread` spawns THREE dedicated
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
//!   thread's inbox;
//! - the **AUTH-release bridge**, which forwards only destructor-free
//!   executor `ReleaseId`s. Rich session/terminal state remains in the
//!   engine-thread registry and is never owned or dropped by the reaper.
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
//! a `HandleId -> RowsSender` registry owned by the engine thread); the
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

mod auth;
mod diagnostics_channel;
mod fifo_channel;
mod row_channel;

pub use auth::{
    AddAuthPolicyError, AuthPolicy, AuthPolicyDecision, AuthPolicyError, AuthPolicyOp,
    AuthPolicyPendingSender, AuthPolicyRegistration, AuthPolicyRequest, AuthPolicyResolveError,
    PendingAuthPolicyOp,
};

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, RecvError, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel as cb;
use nmp_grammar::ConcreteFilter;
use nmp_resolver::{HandleId, LiveQuery};
use nmp_router::{RelayDirectory, SubId, WireDelta, WireOp, WireReq};
use nmp_signer::{PendingSignerOp, SignerOp, SigningCapability};
use nmp_store::EventStore;
use nostr::{
    ClientMessage, Event as SignedEvent, EventId, JsonUtil, PublicKey, RelayMessage, RelayUrl,
    SubscriptionId, Timestamp, UnsignedEvent,
};

use nmp_transport::{
    DurableSendOutcome, HandoffResult, Pool, PoolConfig, PoolEvent, RelayFrame, RelaySessionKey,
    WireFrame,
};

#[doc(hidden)]
pub use crate::core::ReceiptReplayCursor;
use crate::core::{
    self, AcquisitionEvidence, DiagnosticsSnapshot, Effect, EngineCore, EngineMsg,
    HistoryAdvanceError, HistoryBatch, HistoryQuery, HistorySessionId, HistorySink, PublishError,
    ReattachOutcome, ReceiptId, ReceiptSinkRegistration, RelayAdmissionPolicy, Row, RowDelta,
    RowSink,
};
use crate::outbox::{CancelWriteError, CancelWriteOutcome, ReceiptSink, WriteStatus};
use crate::relay_information::{
    RelayInformationCachePolicy, RelayInformationError, RelayInformationService,
    RelayInformationSnapshot,
};
use nmp_grammar::WriteIntent;

use diagnostics_channel::{latest_channel, LatestSender};
pub use diagnostics_channel::{AsyncLatestReceiver, ConcurrentNext, LatestReceiver};
pub use fifo_channel::{
    fifo_channel, AsyncFifoReceiver, FifoNextError, FifoReceiver, FifoRecvError,
    FifoRecvTimeoutError, FifoSender, FifoTryRecvError, FACT_CHANNEL_CAPACITY,
};
use row_channel::{rows_channel, RowsSender};
pub use row_channel::{AsyncRowsReceiver, RowsReceiver};

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
    /// #704: the engine-owned multi-thread tokio runtime that hosts every
    /// adapter task (signer/AUTH completion awaits, NIP-11 fetches, NIP-46
    /// sessions, follow-action). Replaces the deleted blocking-adapter executor.
    runtime: Arc<tokio::runtime::Runtime>,
    relay_information: RelayInformationService,
    max_auth_capabilities: usize,
}

impl nmp_transport::PoolEventSink for EnginePoolSink {
    fn on_event(&self, event: PoolEvent) {
        cb::select_biased! {
            recv(self.stopping) -> _ => {}
            send(self.events, event) -> _ => {}
        }
    }
}

/// One delivered batch for a live subscription: an exact row transition
/// rebased onto the receiver's previous batch + the query's latest per-source
/// acquisition evidence (see [`RowsReceiver`] and the module doc's "two
/// delivery channels" note).
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

    /// Convert to the `Send + Sync` async pull surface (#680). The
    /// receiver-side `delivered` reconcile map moves behind a `Mutex`; the
    /// single-reader guard on the async receiver means `next()` never contends
    /// it with itself.
    pub fn into_async(self) -> AsyncHistoryReceiver {
        AsyncHistoryReceiver {
            batches: AsyncLatestReceiver::new(self.batches),
            delivered: Mutex::new(self.delivered.into_inner()),
        }
    }

    fn reconcile(delivered: &mut BTreeMap<EventId, Row>, mut batch: HistoryBatch) -> HistoryBatch {
        #[cfg(feature = "bench-instrumentation")]
        let reconcile_started = std::time::Instant::now();
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
        #[cfg(feature = "bench-instrumentation")]
        crate::ingest_attribution::history_receiver_reconcile(reconcile_started.elapsed());
        batch
    }
}

/// The async single-consumer half of a bounded, latest-wins history stream
/// (#680). `Send + Sync`: the receiver-side reconcile map is behind a `Mutex`,
/// and the single-reader guard on [`AsyncLatestReceiver`] serialises `next()`.
pub struct AsyncHistoryReceiver {
    batches: AsyncLatestReceiver<HistoryBatch>,
    delivered: Mutex<BTreeMap<EventId, Row>>,
}

impl AsyncHistoryReceiver {
    /// Await the next bounded latest snapshot with exact deltas rebased against
    /// this receiver's last delivered frame, or `None` once the producer is
    /// gone / the consumer cancelled. [`ConcurrentNext`] on an overlapping call.
    pub async fn next(&self) -> Result<Option<HistoryBatch>, ConcurrentNext> {
        match self.batches.next().await? {
            Some(batch) => {
                let mut delivered = self.delivered.lock().unwrap();
                Ok(Some(HistoryReceiver::reconcile(&mut delivered, batch)))
            }
            None => Ok(None),
        }
    }

    /// Idempotent consumer-initiated close; wakes a parked `next()` to `None`.
    pub fn close(&self) {
        self.batches.close();
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
    pub statuses: FifoReceiver<WriteStatus>,
}

/// Result of looking up retained receipt facts by stable id (or, #591, by a
/// caller correlation token translated to one).
pub enum ReceiptReattachment {
    /// The observer is attached and this channel is already primed with all
    /// readable retained facts. Carries the resolved [`ReceiptId`] -- for
    /// [`Handle::reattach_receipt`] this is simply the caller's own input
    /// echoed back; for [`Handle::reattach_by_correlation`] (#591) it is the
    /// id the token resolved to, which the caller could not otherwise learn.
    Attached {
        id: ReceiptId,
        statuses: FifoReceiver<WriteStatus>,
        /// Identity-stable durable-replay continuation for the next finite
        /// page. `None` means this receiver is caught up and attached to
        /// live work.
        next_cursor: Option<ReceiptReplayCursor>,
    },
    /// No retained receipt with this id (or token) exists.
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
struct ChannelReceiptSink(FifoSender<WriteStatus>);

impl ReceiptSink for ChannelReceiptSink {
    fn on_status(&self, status: WriteStatus) -> bool {
        self.0.send(status)
    }
}

fn arm_receipt_sink_close(
    receiver: &FifoReceiver<WriteStatus>,
    inbox: Sender<Cmd>,
    id: ReceiptId,
    registration: ReceiptSinkRegistration,
) {
    receiver.set_close_hook(move || {
        let _ = inbox.send(Cmd::DetachReceiptSink { id, registration });
    });
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
        reply: Sender<Result<(HandleId, RowsReceiver), EngineThreadError>>,
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
        registration: ReceiptSinkRegistration,
        reply: Sender<Result<ReceiptId, PublishError>>,
    },
    ReattachReceipt {
        id: ReceiptId,
        cursor: Option<ReceiptReplayCursor>,
        sink: Box<dyn ReceiptSink>,
        registration: ReceiptSinkRegistration,
        reply: Sender<(ReattachOutcome, Option<ReceiptReplayCursor>)>,
    },
    /// #591: reattach by caller correlation token instead of a `ReceiptId`
    /// -- the door a client uses after a crash that happened before it
    /// could durably record the id `publish_tracked` returned.
    ReattachByCorrelation {
        token: String,
        sink: Box<dyn ReceiptSink>,
        registration: ReceiptSinkRegistration,
        reply: Sender<(
            ReattachOutcome,
            Option<ReceiptId>,
            Option<ReceiptReplayCursor>,
        )>,
    },
    DetachReceiptSink {
        id: ReceiptId,
        registration: ReceiptSinkRegistration,
    },
    #[cfg(test)]
    ReceiptSinkCount {
        id: ReceiptId,
        reply: Sender<usize>,
    },
    CancelWrite {
        id: ReceiptId,
        reply: Sender<Result<CancelWriteOutcome, CancelWriteError>>,
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
    AddAuthPolicy {
        expected_pubkey: PublicKey,
        policy: Box<dyn AuthPolicy>,
        reply: Sender<Result<AuthPolicyRegistration, AddAuthPolicyError>>,
    },
    RemoveAuthPolicy {
        registration: AuthPolicyRegistration,
        reply: Sender<bool>,
    },
    AuthTaskCompleted(auth::AuthTaskCompletion),
    AuthTaskReleased(auth::AuthTaskReleaseToken),
    /// Sign one exact event through the active account's registered
    /// capability without entering the write/store/outbox reducer.
    SignEvent {
        unsigned: UnsignedEvent,
        completion: SignEventCompletion,
        reply: Sender<Result<SignEventRegistration, SignEventError>>,
    },
    CancelSignEvent(u64),
    SignEventFinished(u64),
    /// #704: exempt the exact in-flight sign-event operation whose per-op
    /// completion thread is calling `Engine::join()` reentrantly, keyed by that
    /// operation's id (read from a completion-thread-local).
    ExemptSignEventDrain(u64),
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
/// #704: an idempotent cancel action for one outstanding remote-signer write
/// wait. It wraps the op's `Canceller`; firing it wakes the awaiting async task
/// to a disconnected end and runs the adapter cancel hook once.
type PendingWriteCancel = Box<dyn Fn() + Send>;

#[derive(Default)]
struct SignerRegistry {
    signers: HashMap<PublicKey, RegisteredSigner>,
    pending_writes: RefCell<HashMap<(ReceiptId, u64), PendingWriteCancel>>,
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
    EngineClosed,
    Cancelled,
}

type SignEventCompletion = Box<dyn FnOnce(Result<SignedEvent, SignEventError>) + Send + 'static>;

const SIGN_EVENT_OPEN: u8 = 0;
const SIGN_EVENT_CANCELLED: u8 = 1;
const SIGN_EVENT_RESOLVED: u8 = 2;

thread_local! {
    /// #704: set on a per-operation sign-event completion thread to the exact
    /// operation id it is running. `EngineThread::join()` reads it so a
    /// completion closure that calls `join()` reentrantly exempts only its own
    /// operation from the shutdown drain (replacing the executor `TaskId`
    /// mechanism, which is gone).
    static SIGN_EVENT_COMPLETION_OP: std::cell::Cell<Option<u64>> =
        const { std::cell::Cell::new(None) };
}

/// One linearization point shared by caller cancellation, engine shutdown,
/// runtime shutdown, and signer completion. Cancellation claims `Open ->
/// Cancelled` and fires the bound cancel action (the pending op's canceller for
/// a remote signer; a no-op for a ready local signer).
struct SignEventTerminal {
    state: AtomicU8,
    cancel: Box<dyn Fn() + Send + Sync>,
}

impl SignEventTerminal {
    fn new(cancel: Box<dyn Fn() + Send + Sync>) -> Arc<Self> {
        Arc::new(Self {
            state: AtomicU8::new(SIGN_EVENT_OPEN),
            cancel,
        })
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
        (self.cancel)();
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

struct ActiveSignEvent {
    terminal: Arc<SignEventTerminal>,
}

/// #704: run one foreign sign-event `completion` closure on a FRESH dedicated
/// OS thread spawned for that single in-flight app operation. The closure may
/// block indefinitely and may call `Engine::join()` reentrantly (the
/// reentrant-join tests) — running it on the shared runtime would stall the
/// fixed workers, and a reentrant `join()` from a worker would deadlock tokio.
/// The thread advertises its operation id via `SIGN_EVENT_COMPLETION_OP` so
/// `join()` can exempt exactly this operation, and posts `SignEventFinished`
/// via a drop guard on the way out (panic-safe).
#[allow(clippy::too_many_arguments)]
fn spawn_sign_event_completion(
    inbox: Sender<Cmd>,
    operation_id: u64,
    terminal: Arc<SignEventTerminal>,
    unsigned: UnsignedEvent,
    expected_id: EventId,
    signer_result: Option<Result<SignedEvent, nmp_signer::SignerError>>,
    completion: SignEventCompletion,
) {
    let thread_inbox = inbox.clone();
    let spawned = thread::Builder::new()
        .name("nmp-sign-event-completion".to_string())
        .spawn(move || {
            SIGN_EVENT_COMPLETION_OP.with(|op| op.set(Some(operation_id)));
            let _finished = SignEventFinishedGuard {
                inbox: thread_inbox,
                operation_id,
            };
            let result = match signer_result {
                Some(result) if terminal.resolve() => result
                    .map_err(signer_error)
                    .and_then(|signed| validate_signer_output(&unsigned, expected_id, signed)),
                Some(_) | None => Err(SignEventError::Cancelled),
            };
            completion(result);
        });
    if spawned.is_ok() {
        nmp_executor::note_thread_spawn();
    } else {
        // OS thread exhaustion (astronomically rare): the failed spawn dropped
        // the completion closure without calling it, so the caller observes a
        // disconnected result. Clear the operation from the shutdown drain.
        let _ = inbox.send(Cmd::SignEventFinished(operation_id));
    }
}

/// #704: owns the foreign sign-event `completion` while the async signing wait
/// is outstanding. When the awaiting task resolves it sets `signer_result` and
/// drops; when the task's future is instead dropped (runtime shutdown /
/// cancellation) `signer_result` stays `None`. Either way `Drop` runs the
/// completion exactly once on a fresh per-op OS thread (delivering a signed
/// event, a signer error, or `Cancelled`), never leaving the foreign closure
/// uncalled.
struct SignEventCompletionDispatch {
    inbox: Sender<Cmd>,
    operation_id: u64,
    terminal: Arc<SignEventTerminal>,
    unsigned: UnsignedEvent,
    expected_id: EventId,
    completion: Option<SignEventCompletion>,
    signer_result: Option<Result<SignedEvent, nmp_signer::SignerError>>,
}

impl Drop for SignEventCompletionDispatch {
    fn drop(&mut self) {
        if let Some(completion) = self.completion.take() {
            spawn_sign_event_completion(
                self.inbox.clone(),
                self.operation_id,
                Arc::clone(&self.terminal),
                self.unsigned.clone(),
                self.expected_id,
                self.signer_result.take(),
                completion,
            );
        }
    }
}

struct SignEventFinishedGuard {
    inbox: Sender<Cmd>,
    operation_id: u64,
}

impl Drop for SignEventFinishedGuard {
    fn drop(&mut self) {
        let _ = self.inbox.send(Cmd::SignEventFinished(self.operation_id));
    }
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
    instance: core::AuthCapabilityInstance,
    signer: SharedSigner,
}

type SharedSigner = Arc<Mutex<Box<dyn SigningCapability + Send>>>;

impl SignerRegistry {
    fn contains(&self, pk: PublicKey) -> bool {
        self.signers.contains_key(&pk)
    }

    fn len(&self) -> usize {
        self.signers.len()
    }

    fn track_pending_write(&self, id: ReceiptId, generation: u64, cancel: PendingWriteCancel) {
        if let Some(stale) = self
            .pending_writes
            .borrow_mut()
            .insert((id, generation), cancel)
        {
            stale();
        }
    }

    fn finish_pending_write(&self, id: ReceiptId, generation: u64) {
        self.pending_writes.borrow_mut().remove(&(id, generation));
    }

    fn cancel_pending_write(&self, id: ReceiptId) {
        let mut pending = self.pending_writes.borrow_mut();
        let keys = pending
            .keys()
            .filter(|(receipt, _)| *receipt == id)
            .copied()
            .collect::<Vec<_>>();
        for key in keys {
            if let Some(cancel) = pending.remove(&key) {
                cancel();
            }
        }
    }

    fn cancel_all_pending_writes(&self) {
        for (_, cancel) in self.pending_writes.borrow_mut().drain() {
            cancel();
        }
    }

    /// Register `signer` under its own `public_key()`, replacing any prior
    /// capability already registered for that key.
    fn add(
        &mut self,
        pk: PublicKey,
        instance: core::AuthCapabilityInstance,
        signer: Box<dyn SigningCapability + Send>,
    ) -> (SignerRegistration, Option<core::AuthCapabilityInstance>) {
        let identity = Arc::new(());
        let replaced = self
            .signers
            .insert(
                pk,
                RegisteredSigner {
                    identity: Arc::clone(&identity),
                    instance,
                    signer: Arc::new(Mutex::new(signer)),
                },
            )
            .map(|old| old.instance);
        (
            SignerRegistration {
                public_key: pk,
                identity,
                instance,
            },
            replaced,
        )
    }

    /// Remove only the capability installed by this exact registration.
    /// A stale remote session can therefore never detach a newer replacement
    /// for the same account.
    fn remove(
        &mut self,
        registration: &SignerRegistration,
    ) -> Option<core::AuthCapabilityInstance> {
        let is_current = self
            .signers
            .get(&registration.public_key)
            .is_some_and(|current| {
                current.instance == registration.instance
                    && Arc::ptr_eq(&current.identity, &registration.identity)
            });
        if !is_current {
            return None;
        }
        self.signers
            .remove(&registration.public_key)
            .map(|entry| entry.instance)
    }

    /// Resolve the signer frozen into this exact accepted template. An
    /// account switch cannot redirect already-accepted work.
    fn sign(&self, unsigned: UnsignedEvent) -> Option<SignerOp<SignedEvent>> {
        self.signers.get(&unsigned.pubkey).map(|entry| {
            entry
                .signer
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .sign(unsigned)
        })
    }

    fn auth_snapshot(&self, pk: PublicKey) -> Option<(core::AuthCapabilityInstance, SharedSigner)> {
        self.signers
            .get(&pk)
            .map(|entry| (entry.instance, Arc::clone(&entry.signer)))
    }

    fn is_available(&self, pk: PublicKey) -> bool {
        self.signers.get(&pk).is_some_and(|entry| {
            entry
                .signer
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .is_available()
        })
    }
}

/// One dedicated engine OS thread (§2 position 2) plus the pool and AUTH
/// release bridge threads that feed it. Returned alongside the [`Handle`]
/// the app actually uses; kept around only so a caller (chiefly tests) can
/// deterministically `join` every thread after triggering
/// [`Handle::shutdown`].
pub struct EngineThread {
    engine_join: Option<JoinHandle<()>>,
    bridge_join: Option<JoinHandle<()>>,
    drain_inbox: Sender<Cmd>,
    /// #704: the engine-owned adapter runtime. Shut down from the join thread
    /// (never a worker) after the reducer stops spawning; dropping the last
    /// `Arc` aborts remaining adapter tasks, firing their Drop guards.
    runtime: Arc<tokio::runtime::Runtime>,
    #[cfg(test)]
    runtime_threads: Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(test)]
static RUNTIME_LIFECYCLE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
struct RuntimeThreadCountGuard {
    counter: Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(test)]
impl RuntimeThreadCountGuard {
    fn enter(counter: Arc<std::sync::atomic::AtomicUsize>) -> Self {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self { counter }
    }
}

#[cfg(test)]
impl Drop for RuntimeThreadCountGuard {
    fn drop(&mut self) {
        self.counter
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Supported construction failure for the engine-owned thread graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineThreadError {
    ThreadUnavailable { component: String, reason: String },
    RelayBudgetOverflow { relay_limit: usize },
    EngineShuttingDown,
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
            Self::EngineShuttingDown => f.write_str("engine is shutting down"),
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

pub const DEFAULT_MAX_AUTH_CAPABILITIES: usize = 64;

/// #704: fixed worker-thread count of the ONE engine-owned adapter runtime.
/// Two workers (not one — a single worker makes any accidental blocking call a
/// total outage; not more — the adapter work is µs-scale and every task yields
/// at each `.await`). Every adapter operation is an async task that holds no
/// OS thread while waiting; there is NO admission capacity, census, or
/// per-operation `ThreadUnavailable` anywhere in the SDK.
const ADAPTER_RUNTIME_WORKERS: usize = 2;

/// Finite admission limit for live AUTH policy/signer registrations. Unlike
/// legacy zero-valued relay settings, zero AUTH capabilities intentionally
/// admits none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeConfig {
    pub max_auth_capabilities: usize,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            max_auth_capabilities: DEFAULT_MAX_AUTH_CAPABILITIES,
        }
    }
}

impl EngineThread {
    /// Spawn the engine thread and its two bridge threads. `store`/`directory`
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
        Self::spawn_with_runtime_config(
            store,
            directory,
            cap,
            pool_config,
            admission,
            RuntimeConfig::default(),
        )
    }

    pub fn spawn_with_runtime_config<S, D>(
        store: S,
        directory: D,
        cap: usize,
        mut pool_config: PoolConfig,
        admission: RelayAdmissionPolicy,
        runtime_config: RuntimeConfig,
    ) -> Result<(Self, Handle), EngineThreadError>
    where
        S: EventStore + Send + 'static,
        D: RelayDirectory + Send + 'static,
    {
        // #704: the ONE engine-owned adapter runtime. A fixed 2-worker
        // multi-thread tokio runtime hosts every adapter task; each worker
        // thread start bumps the process-wide OS-thread counter. Build failure
        // is an engine-start infrastructure error.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(ADAPTER_RUNTIME_WORKERS)
            .enable_all()
            .thread_name("nmp-adapter")
            .on_thread_start(nmp_executor::note_thread_spawn)
            .build()
            .map(Arc::new)
            .map_err(|error| EngineThreadError::ThreadUnavailable {
                component: "adapter runtime".to_string(),
                reason: error.to_string(),
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
            runtime.handle().clone(),
            Arc::clone(&allowed_local_hosts),
        );
        #[cfg(test)]
        let runtime_threads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let max_engine_batch = pool_config.max_engine_batch.max(1);
        let max_engine_batch_bytes = pool_config.max_engine_batch_bytes.max(1);
        let max_engine_batch_wait = pool_config
            .max_engine_batch_wait
            .min(Duration::from_millis(100));
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
                return Err(pool_build_error(error));
            }
        };

        let bridge_inbox = cmd_tx.clone();
        #[cfg(test)]
        let bridge_runtime_threads = Arc::clone(&runtime_threads);
        let bridge_join = match thread::Builder::new()
            .name("nmp-engine-pool-bridge".to_string())
            .spawn(move || {
                #[cfg(test)]
                let _thread_count = RuntimeThreadCountGuard::enter(bridge_runtime_threads);
                pool_bridge_loop(
                    &pool_evt_rx,
                    &pool_stop_rx,
                    &bridge_inbox,
                    max_engine_batch,
                    max_engine_batch_bytes,
                    max_engine_batch_wait,
                )
            }) {
            Ok(join) => {
                nmp_executor::note_thread_spawn();
                join
            }
            Err(error) => {
                pool.shutdown();
                return Err(EngineThreadError::ThreadUnavailable {
                    component: "engine pool bridge".to_string(),
                    reason: error.to_string(),
                });
            }
        };

        let self_inbox = cmd_tx.clone();
        let engine_pool = pool.clone();
        let engine_stop = pool_stop_tx.clone();
        let engine_runtime = Arc::clone(&runtime);
        let engine_relay_information = relay_information.clone();
        #[cfg(test)]
        let engine_runtime_threads = Arc::clone(&runtime_threads);
        let engine_join =
            match thread::Builder::new()
                .name("nmp-engine".to_string())
                .spawn(move || {
                    #[cfg(test)]
                    let _thread_count = RuntimeThreadCountGuard::enter(engine_runtime_threads);
                    engine_loop(
                        store,
                        directory,
                        cap,
                        admission,
                        EnginePoolRuntime {
                            pool: engine_pool,
                            stop: engine_stop,
                            runtime: engine_runtime,
                            relay_information: engine_relay_information,
                            max_auth_capabilities: runtime_config.max_auth_capabilities,
                        },
                        &cmd_rx,
                        &self_inbox,
                    )
                }) {
                Ok(join) => {
                    nmp_executor::note_thread_spawn();
                    join
                }
                Err(error) => {
                    drop(pool_stop_tx);
                    pool.shutdown();
                    let _ = bridge_join.join();
                    return Err(EngineThreadError::ThreadUnavailable {
                        component: "engine runtime".to_string(),
                        reason: error.to_string(),
                    });
                }
            };
        drop(pool);

        Ok((
            Self {
                engine_join: Some(engine_join),
                bridge_join: Some(bridge_join),
                drain_inbox: cmd_tx.clone(),
                runtime,
                #[cfg(test)]
                runtime_threads,
            },
            Handle {
                inbox: cmd_tx,
                relay_information,
            },
        ))
    }

    /// #704: the engine-owned adapter runtime handle. Protocol adapters
    /// (NIP-02 follow-action, NIP-46 connect handshakes) spawn their async
    /// tasks here instead of reserving a slot on the deleted blocking-adapter
    /// executor. Exposed on [`EngineThread`] (not the narrow app-facing
    /// [`Handle`]) so it stays hidden mechanism, never an app scheduling verb.
    #[must_use]
    pub fn adapter_runtime(&self) -> tokio::runtime::Handle {
        self.runtime.handle().clone()
    }

    /// Block until the engine and both bridge threads have exited. Only
    /// returns once a [`Handle::shutdown`] has actually been observed by the
    /// engine thread (which then tears down its `Pool` clone, allowing the
    /// pool bridge to disconnect) — callers that never shut down any `Handle`
    /// block here forever, matching `Pool::shutdown`'s own join discipline.
    ///
    /// #704: when called from a per-operation sign-event completion thread that
    /// is calling `join()` reentrantly, the reducer exempts only that exact
    /// operation from the shutdown drain (read from the completion-thread-local
    /// `SIGN_EVENT_COMPLETION_OP`). The adapter runtime is then shut down from
    /// THIS join thread (never a worker) by dropping the last `Arc` after the
    /// reducer thread has exited — remaining adapter task futures are dropped,
    /// firing their Drop guards (delivering `Cancelled`/`Disconnected` to any
    /// foreign completion exactly once).
    pub fn join(mut self) {
        if let Some(op_id) = SIGN_EVENT_COMPLETION_OP.with(|op| op.get()) {
            let _ = self.drain_inbox.send(Cmd::ExemptSignEventDrain(op_id));
        }
        if let Some(h) = self.engine_join.take() {
            let _ = h.join();
        }
        if let Some(h) = self.bridge_join.take() {
            let _ = h.join();
        }
        // The reducer thread has exited (its runtime `Arc` clone dropped), so
        // this is the last `Arc`. Shut the adapter runtime down on a FRESH
        // dedicated OS thread and join it: dropping a `tokio::runtime::Runtime`
        // panics if done inside another runtime's context (e.g. an app or a
        // `#[tokio::test]` that owns the calling thread), and `join()` may be
        // called from exactly there. On the fresh thread the drop is legal and
        // fires every parked adapter task's Drop guard, delivering
        // `Cancelled`/`Disconnected` to each foreign completion exactly once.
        let runtime = self.runtime;
        let _ = thread::Builder::new()
            .name("nmp-adapter-shutdown".to_string())
            .spawn(move || drop(runtime))
            .map(|handle| handle.join());
    }
}

#[cfg(test)]
mod receipt_sink_lifecycle_tests {
    use super::*;
    use nmp_grammar::{Durability, WriteIntent, WritePayload, WriteRouting};
    use nmp_router::FixtureDirectory;
    use nmp_store::MemoryStore;
    use nostr::{Keys, Kind, UnsignedEvent};

    fn parked_write(handle: &Handle, keys: &Keys) -> ReceiptStream {
        handle.set_active_account(Some(keys.public_key()));
        handle
            .publish_tracked(WriteIntent {
                payload: WritePayload::Unsigned(UnsignedEvent::new(
                    keys.public_key(),
                    Timestamp::now(),
                    Kind::TextNote,
                    vec![],
                    "parked receipt sink lifecycle",
                )),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
                identity_override: None,
                correlation: None,
            })
            .expect("parked write is accepted")
    }

    /// A receipt without its signer may never emit another live status. Stream
    /// cancel/drop must therefore remove its exact observer immediately,
    /// without relying on a later `notify` call to prune a closed mailbox.
    #[test]
    fn parked_awaiting_capability_reattach_cancel_does_not_retain_sinks() {
        let (thread, handle) = EngineThread::spawn(
            MemoryStore::new(),
            FixtureDirectory::new(),
            10,
            PoolConfig::default(),
            RelayAdmissionPolicy::new(["127.0.0.1".to_string()]),
        )
        .expect("test engine thread construction");
        let tracked = parked_write(&handle, &Keys::generate());
        let id = tracked.id;
        assert_eq!(handle.receipt_sink_count(id), 1);

        tracked.statuses.close();
        assert_eq!(
            handle.receipt_sink_count(id),
            0,
            "closing the original publish stream withdraws its observer"
        );

        for iteration in 0..128 {
            let statuses = match handle.reattach_receipt(id) {
                ReceiptReattachment::Attached {
                    statuses,
                    next_cursor: None,
                    ..
                } => statuses,
                ReceiptReattachment::Attached { .. } => {
                    panic!("two retained facts fit in one replay page")
                }
                ReceiptReattachment::NotFound | ReceiptReattachment::RetainedButUnreadable => {
                    panic!("parked retained receipt remains readable")
                }
            };
            assert_eq!(
                handle.receipt_sink_count(id),
                1,
                "each fresh reattachment owns exactly one live observer"
            );
            if iteration % 2 == 0 {
                statuses.close();
            } else {
                drop(statuses);
            }
            assert_eq!(
                handle.receipt_sink_count(id),
                0,
                "cancel/drop detaches before the next engine command"
            );
        }

        handle.shutdown();
        thread.join();
    }
}

#[cfg(test)]
mod reentrant_shutdown_tests {
    use super::*;
    use nmp_router::FixtureDirectory;
    use nmp_signer::LocalKeySigner;
    use nmp_store::MemoryStore;
    use nostr::{Keys, Kind};

    fn runtime() -> (EngineThread, Handle) {
        // #680 removed the configurable native-task limit; the blocking-adapter
        // pool is a fixed internal capacity, so spawn takes no limit argument.
        EngineThread::spawn(
            MemoryStore::new(),
            FixtureDirectory::new(),
            1,
            PoolConfig::default(),
            RelayAdmissionPolicy::default(),
        )
        .expect("engine construction")
    }

    fn unsigned(keys: &Keys, content: &str) -> UnsignedEvent {
        UnsignedEvent::new(
            keys.public_key(),
            Timestamp::from(1),
            Kind::TextNote,
            Vec::new(),
            content.to_string(),
        )
    }

    #[test]
    fn external_shutdown_first_then_callback_owned_join_exempts_exact_origin() {
        let (engine, handle) = runtime();
        let keys = Keys::generate();
        handle
            .add_signer(LocalKeySigner::new(keys.clone()))
            .expect("signer registration");
        handle.set_active_account(Some(keys.public_key()));

        let (entered_tx, entered_rx) = mpsc::channel();
        let (join_tx, join_rx) = mpsc::channel();
        let (returned_tx, returned_rx) = mpsc::channel();
        handle
            .sign_event_with_completion(unsigned(&keys, "reentrant shutdown"), move |result| {
                assert!(result.is_ok(), "local signer must complete: {result:?}");
                let _ = entered_tx.send(());
                let _ = join_rx.recv();
                engine.join();
                let _ = returned_tx.send(());
            })
            .expect("sign-event admission");

        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("completion callback must start");
        handle.shutdown();
        assert_eq!(
            handle.add_signer(LocalKeySigner::new(Keys::generate())),
            Err(AddSignerError::EngineShuttingDown),
            "the external shutdown must enter its drain before callback-owned join"
        );
        join_tx.send(()).expect("allow callback-owned join");

        returned_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("callback-owned join must exempt itself after shutdown already began");
    }

    #[test]
    fn callback_handle_shutdown_does_not_weaken_external_join_drain() {
        let (engine, handle) = runtime();
        let keys = Keys::generate();
        handle
            .add_signer(LocalKeySigner::new(keys.clone()))
            .expect("signer registration");
        handle.set_active_account(Some(keys.public_key()));

        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let callback_handle = handle.clone();
        handle
            .sign_event_with_completion(unsigned(&keys, "external shutdown"), move |result| {
                assert!(result.is_ok(), "local signer must complete: {result:?}");
                callback_handle.shutdown();
                let _ = entered_tx.send(());
                let _ = release_rx.recv();
            })
            .expect("sign-event admission");
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("completion callback must start");
        assert_eq!(
            handle.add_signer(LocalKeySigner::new(Keys::generate())),
            Err(AddSignerError::EngineShuttingDown),
            "callback shutdown must enter its drain before external join"
        );

        let (returned_tx, returned_rx) = mpsc::channel();
        let shutdown = thread::spawn(move || {
            engine.join();
            let _ = returned_tx.send(());
        });
        assert!(
            returned_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "a callback Handle::shutdown cannot exempt an externally-owned join"
        );
        release_tx.send(()).expect("release callback");
        returned_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("external shutdown must return after callback completion");
        shutdown.join().expect("shutdown thread");
    }

    #[test]
    fn panicking_callback_still_finishes_external_shutdown_drain() {
        let (engine, handle) = runtime();
        let keys = Keys::generate();
        handle
            .add_signer(LocalKeySigner::new(keys.clone()))
            .expect("signer registration");
        handle.set_active_account(Some(keys.public_key()));

        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        handle
            .sign_event_with_completion(unsigned(&keys, "panicking callback"), move |result| {
                assert!(result.is_ok(), "local signer must complete: {result:?}");
                let _ = entered_tx.send(());
                let _ = release_rx.recv();
                panic!("injected completion panic");
            })
            .expect("sign-event admission");
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("completion callback must start");

        handle.shutdown();
        assert_eq!(
            handle.add_signer(LocalKeySigner::new(Keys::generate())),
            Err(AddSignerError::EngineShuttingDown),
            "external shutdown must enter its drain before the callback panics"
        );
        let (returned_tx, returned_rx) = mpsc::channel();
        let shutdown = thread::spawn(move || {
            engine.join();
            let _ = returned_tx.send(());
        });
        assert!(
            returned_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "external join must retain the callback until its panic unwinds"
        );
        release_tx.send(()).expect("release callback to panic");
        returned_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("panic-safe Finished guard must release the shutdown drain");
        shutdown.join().expect("shutdown thread");
    }

    #[test]
    fn callback_owned_join_exempts_only_itself_and_drains_another_callback() {
        let (engine, handle) = runtime();
        let keys = Keys::generate();
        handle
            .add_signer(LocalKeySigner::new(keys.clone()))
            .expect("signer registration");
        handle.set_active_account(Some(keys.public_key()));

        let (other_entered_tx, other_entered_rx) = mpsc::channel();
        let (release_other_tx, release_other_rx) = mpsc::channel();
        handle
            .sign_event_with_completion(unsigned(&keys, "other callback"), move |result| {
                assert!(result.is_ok(), "local signer must complete: {result:?}");
                let _ = other_entered_tx.send(());
                let _ = release_other_rx.recv();
            })
            .expect("other sign-event admission");
        other_entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("other callback must start");

        let callback_handle = handle.clone();
        let (joining_entered_tx, joining_entered_rx) = mpsc::channel();
        let (returned_tx, returned_rx) = mpsc::channel();
        handle
            .sign_event_with_completion(unsigned(&keys, "joining callback"), move |result| {
                assert!(result.is_ok(), "local signer must complete: {result:?}");
                callback_handle.shutdown();
                let _ = joining_entered_tx.send(());
                engine.join();
                let _ = returned_tx.send(());
            })
            .expect("joining sign-event admission");

        joining_entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("joining callback must start");
        assert!(
            returned_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "exact-origin exemption must retain every other callback in the drain"
        );
        release_other_tx.send(()).expect("release other callback");
        returned_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("callback-owned join must return after the other callback completes");
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
    max_engine_batch_bytes: usize,
    max_engine_batch_wait: Duration,
) {
    let mut pending = None;
    loop {
        let event = match pending.take() {
            Some(event) => event,
            None => cb::select_biased! {
                recv(stopping) -> _ => break,
                recv(pool_evt_rx) -> event => match event {
                    Ok(event) => event,
                    Err(_) => break,
                },
            },
        };
        if let PoolEvent::Frame {
            handle,
            session,
            frame,
        } = event
        {
            let Some(first_bytes) = encoded_event_upper_bound(&frame) else {
                if !send_relay_batch(vec![(handle, session, frame)], stopping, engine_inbox) {
                    break;
                }
                continue;
            };
            let mut frames = vec![(handle, session, frame)];
            let mut encoded_bytes = first_bytes;
            let deadline = std::time::Instant::now()
                .checked_add(max_engine_batch_wait)
                .unwrap_or_else(std::time::Instant::now);
            let mut input_closed = false;
            let mut stopped = false;
            loop {
                if frames.len() >= max_engine_batch || encoded_bytes >= max_engine_batch_bytes {
                    break;
                }
                let next = match pool_evt_rx.try_recv() {
                    Ok(event) => Some(event),
                    Err(cb::TryRecvError::Disconnected) => {
                        input_closed = true;
                        None
                    }
                    Err(cb::TryRecvError::Empty) => {
                        let remaining =
                            deadline.saturating_duration_since(std::time::Instant::now());
                        if remaining.is_zero() {
                            None
                        } else {
                            let timeout = cb::after(remaining);
                            cb::select_biased! {
                                recv(stopping) -> _ => {
                                    stopped = true;
                                    None
                                },
                                recv(pool_evt_rx) -> event => match event {
                                    Ok(event) => Some(event),
                                    Err(_) => {
                                        input_closed = true;
                                        None
                                    },
                                },
                                recv(timeout) -> _ => None,
                            }
                        }
                    }
                };
                let Some(next) = next else { break };
                let PoolEvent::Frame {
                    handle,
                    session,
                    frame,
                } = next
                else {
                    pending = Some(next);
                    break;
                };
                let Some(next_bytes) = encoded_event_upper_bound(&frame) else {
                    pending = Some(PoolEvent::Frame {
                        handle,
                        session,
                        frame,
                    });
                    break;
                };
                if encoded_bytes.saturating_add(next_bytes) > max_engine_batch_bytes {
                    pending = Some(PoolEvent::Frame {
                        handle,
                        session,
                        frame,
                    });
                    break;
                }
                encoded_bytes = encoded_bytes.saturating_add(next_bytes);
                frames.push((handle, session, frame));
            }
            if stopped || !send_relay_batch(frames, stopping, engine_inbox) {
                break;
            }
            if input_closed {
                break;
            }
            continue;
        }
        if !forward_pool_event(event, engine_inbox) {
            break; // engine thread is gone; nothing left to feed.
        }
    }
}

fn send_relay_batch(
    frames: Vec<(nmp_transport::RelayHandle, RelaySessionKey, RelayFrame)>,
    stopping: &cb::Receiver<()>,
    engine_inbox: &Sender<Cmd>,
) -> bool {
    let (applied_tx, applied_rx) = cb::bounded(1);
    #[cfg(feature = "bench-instrumentation")]
    {
        let event_bytes = frames
            .iter()
            .filter_map(|(_, _, frame)| encoded_event_upper_bound(frame))
            .fold(0usize, usize::saturating_add);
        crate::ingest_attribution::bridge_batch(frames.len(), event_bytes);
    }
    #[cfg(feature = "bench-instrumentation")]
    let send_started = std::time::Instant::now();
    if engine_inbox
        .send(Cmd::RelayBatch {
            frames,
            applied: applied_tx,
        })
        .is_err()
    {
        return false;
    }
    #[cfg(feature = "bench-instrumentation")]
    crate::ingest_attribution::bridge_send(send_started.elapsed());
    #[cfg(feature = "bench-instrumentation")]
    let applied_started = std::time::Instant::now();
    let applied = cb::select_biased! {
        recv(stopping) -> _ => false,
        recv(applied_rx) -> result => result.is_ok(),
    };
    #[cfg(feature = "bench-instrumentation")]
    crate::ingest_attribution::bridge_applied_wait(applied_started.elapsed());
    applied
}

fn encoded_event_upper_bound(frame: &RelayFrame) -> Option<usize> {
    if let RelayFrame::CommittedObservation(hit) = frame {
        return Some(hit.encoded_bytes());
    }
    #[cfg(feature = "bench-instrumentation")]
    if let Some((_, encoded_bytes)) = frame.diagnostic_duplicate_ceiling() {
        return Some(encoded_bytes);
    }
    let event = frame.event()?;
    let tags = event.tags.iter().fold(0usize, |total, tag| {
        tag.as_slice()
            .iter()
            .fold(total.saturating_add(4), |total, atom| {
                total.saturating_add(4).saturating_add(atom.len())
            })
    });
    Some(
        192usize
            .saturating_add(event.content.len())
            .saturating_add(tags),
    )
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
    use nostr::{EventBuilder, Keys, RelayMessage, SubscriptionId};

    fn notice_frame(text: &str) -> RelayFrame {
        RelayFrame::from_message(RelayMessage::notice(text))
    }

    fn event_frame(text: &str) -> RelayFrame {
        let event = EventBuilder::text_note(text)
            .sign_with_keys(&Keys::generate())
            .unwrap();
        RelayFrame::from_message(RelayMessage::event(SubscriptionId::new("sub"), event))
    }

    fn test_session() -> RelaySessionKey {
        RelaySessionKey::public(RelayUrl::parse("wss://relay.example.com").unwrap())
    }

    fn protected_session() -> RelaySessionKey {
        RelaySessionKey::new(
            RelayUrl::parse("wss://relay.example.com").unwrap(),
            nmp_grammar::AccessContext::Nip42(nostr::Keys::generate().public_key()),
        )
    }

    #[test]
    fn buffered_auth_batch_is_applied_before_initial_read_release() {
        let (pool_tx, pool_rx) = cb::bounded(8);
        let (stop_tx, stop_rx) = cb::bounded(0);
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let handle = RelayHandle {
            slot: 1,
            generation: 2,
        };
        let session = protected_session();
        pool_tx
            .send(PoolEvent::Connected {
                handle,
                session: session.clone(),
            })
            .unwrap();
        pool_tx
            .send(PoolEvent::Frame {
                handle,
                session: session.clone(),
                frame: RelayFrame::from_message(RelayMessage::Auth {
                    challenge: "bridge-ordered".into(),
                }),
            })
            .unwrap();
        pool_tx
            .send(PoolEvent::InitialReadCompleted {
                handle,
                session: session.clone(),
            })
            .unwrap();
        let bridge = thread::spawn(move || {
            pool_bridge_loop(&pool_rx, &stop_rx, &cmd_tx, 128, usize::MAX, Duration::ZERO)
        });

        assert!(matches!(
            cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Cmd::Engine(EngineMsg::RelayConnected(current, ref current_session))
                if current == handle && *current_session == session
        ));
        let applied = match cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            Cmd::RelayBatch { frames, applied } => {
                assert_eq!(frames.len(), 1);
                assert!(relay_frame_is_auth(&frames[0].2));
                applied
            }
            _ => panic!("AUTH must enter the reducer as a relay batch"),
        };
        assert!(
            matches!(cmd_rx.try_recv(), Err(mpsc::TryRecvError::Empty)),
            "the completion edge cannot overtake the stalled AUTH reduction"
        );
        applied.send(()).unwrap();
        assert!(matches!(
            cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Cmd::Engine(EngineMsg::AuthProbeReleased(current, ref current_session))
                if current == handle && *current_session == session
        ));

        drop(pool_tx);
        drop(stop_tx);
        bridge.join().unwrap();
    }

    #[test]
    fn bridge_waits_for_applied_ack_before_enqueuing_another_relay_batch() {
        let (pool_tx, pool_rx) = cb::bounded(8);
        let (stop_tx, stop_rx) = cb::bounded(0);
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let bridge = thread::spawn(move || {
            pool_bridge_loop(&pool_rx, &stop_rx, &cmd_tx, 128, usize::MAX, Duration::ZERO)
        });
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
                    frame: event_frame(text),
                })
                .unwrap();
        }
        let bridge = thread::spawn(move || {
            pool_bridge_loop(&pool_rx, &stop_rx, &cmd_tx, 2, usize::MAX, Duration::ZERO)
        });

        let first_ack = match cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            Cmd::RelayBatch { frames, applied } => {
                assert_eq!(frames.len(), 2);
                assert_eq!(frames[0].2.event().unwrap().content, "one");
                assert_eq!(frames[1].2.event().unwrap().content, "two");
                applied
            }
            _ => panic!("first command must be a capped relay batch"),
        };
        first_ack.send(()).unwrap();
        let second_ack = match cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            Cmd::RelayBatch { frames, applied } => {
                assert_eq!(frames.len(), 1);
                assert_eq!(frames[0].2.event().unwrap().content, "three");
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
    fn control_frame_is_a_commit_barrier_between_event_batches() {
        let (pool_tx, pool_rx) = cb::bounded(8);
        let (stop_tx, stop_rx) = cb::bounded(0);
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let handle = RelayHandle {
            slot: 1,
            generation: 2,
        };
        for frame in [
            event_frame("before"),
            notice_frame("barrier"),
            event_frame("after"),
        ] {
            pool_tx
                .send(PoolEvent::Frame {
                    handle,
                    session: test_session(),
                    frame,
                })
                .unwrap();
        }
        let bridge = thread::spawn(move || {
            pool_bridge_loop(&pool_rx, &stop_rx, &cmd_tx, 8, usize::MAX, Duration::ZERO)
        });

        let before_ack = match cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            Cmd::RelayBatch { frames, applied } => {
                assert_eq!(frames.len(), 1);
                assert_eq!(frames[0].2.event().unwrap().content, "before");
                applied
            }
            _ => panic!("event before barrier must commit first"),
        };
        assert!(matches!(cmd_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        before_ack.send(()).unwrap();

        let barrier_ack = match cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            Cmd::RelayBatch { frames, applied } => {
                assert_eq!(frames.len(), 1);
                assert_eq!(
                    frames[0].2.clone().into_message(),
                    RelayMessage::notice("barrier")
                );
                applied
            }
            _ => panic!("control barrier must be applied after prior commit"),
        };
        assert!(matches!(cmd_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        barrier_ack.send(()).unwrap();

        let after_ack = match cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            Cmd::RelayBatch { frames, applied } => {
                assert_eq!(frames.len(), 1);
                assert_eq!(frames[0].2.event().unwrap().content, "after");
                applied
            }
            _ => panic!("event after barrier must remain ordered"),
        };
        after_ack.send(()).unwrap();
        drop(pool_tx);
        drop(stop_tx);
        bridge.join().unwrap();
    }

    #[test]
    fn lifecycle_event_is_a_commit_barrier_between_event_batches() {
        let (pool_tx, pool_rx) = cb::bounded(8);
        let (stop_tx, stop_rx) = cb::bounded(0);
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let handle = RelayHandle {
            slot: 1,
            generation: 2,
        };
        pool_tx
            .send(PoolEvent::Frame {
                handle,
                session: test_session(),
                frame: event_frame("before"),
            })
            .unwrap();
        pool_tx.send(PoolEvent::WorkerRetired).unwrap();
        pool_tx
            .send(PoolEvent::Frame {
                handle,
                session: test_session(),
                frame: event_frame("after"),
            })
            .unwrap();
        let bridge = thread::spawn(move || {
            pool_bridge_loop(&pool_rx, &stop_rx, &cmd_tx, 8, usize::MAX, Duration::ZERO)
        });

        let before_ack = match cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            Cmd::RelayBatch { frames, applied } => {
                assert_eq!(frames.len(), 1);
                assert_eq!(frames[0].2.event().unwrap().content, "before");
                applied
            }
            _ => panic!("event before lifecycle barrier must commit first"),
        };
        assert!(matches!(cmd_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        before_ack.send(()).unwrap();

        assert!(matches!(
            cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            Cmd::RelayWorkerRetired
        ));
        let after_ack = match cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            Cmd::RelayBatch { frames, applied } => {
                assert_eq!(frames.len(), 1);
                assert_eq!(frames[0].2.event().unwrap().content, "after");
                applied
            }
            _ => panic!("event after lifecycle barrier must remain ordered"),
        };
        after_ack.send(()).unwrap();
        drop(pool_tx);
        drop(stop_tx);
        bridge.join().unwrap();
    }

    #[test]
    fn encoded_byte_bound_splits_consecutive_events_without_loss() {
        let first = event_frame(&"a".repeat(512));
        let second = event_frame(&"b".repeat(512));
        let one_event_bytes = encoded_event_upper_bound(&first).unwrap();
        let (pool_tx, pool_rx) = cb::bounded(4);
        let (stop_tx, stop_rx) = cb::bounded(0);
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let handle = RelayHandle {
            slot: 1,
            generation: 2,
        };
        for frame in [first, second] {
            pool_tx
                .send(PoolEvent::Frame {
                    handle,
                    session: test_session(),
                    frame,
                })
                .unwrap();
        }
        let bridge = thread::spawn(move || {
            pool_bridge_loop(
                &pool_rx,
                &stop_rx,
                &cmd_tx,
                8,
                one_event_bytes + 1,
                Duration::ZERO,
            )
        });
        for expected in ['a', 'b'] {
            let ack = match cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
                Cmd::RelayBatch { frames, applied } => {
                    assert_eq!(frames.len(), 1);
                    assert!(frames[0].2.event().unwrap().content.starts_with(expected));
                    applied
                }
                _ => panic!("byte bound must preserve each event"),
            };
            ack.send(()).unwrap();
        }
        drop(pool_tx);
        drop(stop_tx);
        bridge.join().unwrap();
    }

    #[test]
    fn bounded_wait_coalesces_a_short_event_burst() {
        let (pool_tx, pool_rx) = cb::bounded(4);
        let (stop_tx, stop_rx) = cb::bounded(0);
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let handle = RelayHandle {
            slot: 1,
            generation: 2,
        };
        let bridge = thread::spawn(move || {
            pool_bridge_loop(
                &pool_rx,
                &stop_rx,
                &cmd_tx,
                8,
                usize::MAX,
                Duration::from_millis(50),
            )
        });
        pool_tx
            .send(PoolEvent::Frame {
                handle,
                session: test_session(),
                frame: event_frame("first"),
            })
            .unwrap();
        thread::sleep(Duration::from_millis(5));
        pool_tx
            .send(PoolEvent::Frame {
                handle,
                session: test_session(),
                frame: event_frame("second"),
            })
            .unwrap();
        let ack = match cmd_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            Cmd::RelayBatch { frames, applied } => {
                assert_eq!(frames.len(), 2);
                applied
            }
            _ => panic!("short burst must coalesce"),
        };
        ack.send(()).unwrap();
        drop(pool_tx);
        drop(stop_tx);
        bridge.join().unwrap();
    }

    #[test]
    fn stop_disconnect_releases_bridge_waiting_for_engine_ack() {
        let (pool_tx, pool_rx) = cb::bounded(1);
        let (stop_tx, stop_rx) = cb::bounded(0);
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let bridge = thread::spawn(move || {
            pool_bridge_loop(&pool_rx, &stop_rx, &cmd_tx, 1, usize::MAX, Duration::ZERO)
        });
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

    use nmp_grammar::{
        AccessContext, Binding, Demand, Durability, Filter, SourceAuthority, WriteIntent,
        WritePayload, WriteRouting,
    };
    use nmp_router::FixtureDirectory;
    use nmp_store::MemoryStore;
    use nostr::{Keys, Kind, UnsignedEvent};

    struct NullReceiptSink;

    impl ReceiptSink for NullReceiptSink {
        fn on_status(&self, _status: WriteStatus) -> bool {
            true
        }
    }

    fn query(author: &str) -> LiveQuery {
        LiveQuery::from_filter(Filter {
            kinds: Some(BTreeSet::from([1])),
            authors: Some(Binding::Literal(BTreeSet::from([author.to_string()]))),
            ..Filter::default()
        })
    }

    fn protected_query(relay: &RelayUrl, signer: PublicKey, kind: u16) -> LiveQuery {
        LiveQuery(
            Demand::new(
                Filter {
                    kinds: Some(BTreeSet::from([kind])),
                    ..Filter::default()
                },
                SourceAuthority::Pinned(BTreeSet::from([relay.clone()])),
                AccessContext::Nip42(signer),
            )
            .expect("protected pinned query"),
        )
    }

    #[test]
    fn fresh_protected_read_opens_one_worker_without_a_req_preamble_and_releases_on_withdrawal() {
        let signer = Keys::generate().public_key();
        let relay = RelayUrl::parse("ws://127.0.0.1:9").unwrap();
        let session = RelaySessionKey::new(relay.clone(), AccessContext::Nip42(signer));
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 1);
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
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let relay_information = RelayInformationService::new(rt.handle().clone());
        let nip11_decisions = RefCell::new(Nip11DecisionState::default());
        let auth_policies = RefCell::new(auth::AuthPolicyRegistry::default());
        let auth_tasks = RefCell::new(auth::AuthTaskRegistry::default());
        let dispatch_runtime = DispatchRuntime {
            self_inbox: &self_inbox,
            relay_information: &relay_information,
            runtime: rt.handle(),
            nip11_decisions: &nip11_decisions,
            auth_policies: &auth_policies,
            auth_tasks: &auth_tasks,
        };

        let first = core.handle(EngineMsg::Subscribe(
            protected_query(&relay, signer, 1),
            Box::new(NullRowSink),
        ));
        let first_id = first
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitRows(id, ..) => Some(*id),
                _ => None,
            })
            .expect("first subscription handle");
        assert_eq!(
            first
                .iter()
                .filter(|effect| matches!(effect, Effect::EnsureRelay(candidate) if candidate == &session))
                .count(),
            1
        );
        assert!(!first.iter().any(|effect| matches!(
            effect,
            Effect::Wire(delta)
                if delta.ops.iter().any(|(candidate, ops)| candidate == &session
                    && ops.iter().any(|op| matches!(op, WireOp::Req(..))))
        )));
        preflight_query_relay_workers(&first, &pool)
            .expect("protected worker is acquired before the subscribe reply");
        let first_transport = pool
            .live_session_handle(&session)
            .expect("preflight opens the protected worker");
        dispatch_core_effects(
            &mut core,
            first,
            &pool,
            &mut rows,
            &mut histories,
            &mut diagnostics,
            &mut preambles,
            &registry,
            dispatch_runtime,
        );
        assert_eq!(pool.live_session_handle(&session), Some(first_transport));
        assert!(!preambles.contains_key(&session));

        let second = core.handle(EngineMsg::Subscribe(
            protected_query(&relay, signer, 2),
            Box::new(NullRowSink),
        ));
        let second_id = second
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitRows(id, ..) => Some(*id),
                _ => None,
            })
            .expect("second subscription handle");
        dispatch_core_effects(
            &mut core,
            second,
            &pool,
            &mut rows,
            &mut histories,
            &mut diagnostics,
            &mut preambles,
            &registry,
            dispatch_runtime,
        );
        assert_eq!(pool.live_session_handle(&session), Some(first_transport));
        assert_eq!(pool.admission_rejections(), 0);
        assert!(!preambles.contains_key(&session));

        let newest_only = core.handle(EngineMsg::Unsubscribe(first_id));
        dispatch_core_effects(
            &mut core,
            newest_only,
            &pool,
            &mut rows,
            &mut histories,
            &mut diagnostics,
            &mut preambles,
            &registry,
            dispatch_runtime,
        );
        assert_eq!(pool.live_session_handle(&session), Some(first_transport));

        let removed = core.handle(EngineMsg::Unsubscribe(second_id));
        dispatch_core_effects(
            &mut core,
            removed,
            &pool,
            &mut rows,
            &mut histories,
            &mut diagnostics,
            &mut preambles,
            &registry,
            dispatch_runtime,
        );
        assert!(pool.live_session_handle(&session).is_none());

        pool.shutdown();
    }

    #[test]
    fn protected_initial_subscribe_spawn_refusal_rolls_back_every_owned_layer() {
        let signer = Keys::generate().public_key();
        let first_relay = RelayUrl::parse("ws://127.0.0.1:9").unwrap();
        let refused_relay = RelayUrl::parse("ws://127.0.0.1:10").unwrap();
        let access = AccessContext::Nip42(signer);
        let first_session = RelaySessionKey::new(first_relay.clone(), access);
        let refused_session = RelaySessionKey::new(refused_relay.clone(), access);
        let query = LiveQuery(
            Demand::new(
                Filter {
                    kinds: Some(BTreeSet::from([1])),
                    ..Filter::default()
                },
                SourceAuthority::Pinned(BTreeSet::from([
                    first_relay.clone(),
                    refused_relay.clone(),
                ])),
                access,
            )
            .unwrap(),
        );
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 2);
        let (pool_tx, _pool_rx) = mpsc::channel();
        let mut config = PoolConfig::default();
        config.max_relays = 2;
        let pool = Pool::new(config, pool_tx).expect("test pool construction");
        let mut rows = HashMap::new();
        let mut histories = HashMap::new();
        let mut diagnostics = HashMap::new();
        let mut preambles = Preambles::new();
        let registry = SignerRegistry::default();
        let (self_inbox, _inbox_rx) = mpsc::channel();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let relay_information = RelayInformationService::new(rt.handle().clone());
        let nip11_decisions = RefCell::new(Nip11DecisionState::default());
        let auth_policies = RefCell::new(auth::AuthPolicyRegistry::default());
        let auth_tasks = RefCell::new(auth::AuthTaskRegistry::default());
        let dispatch_runtime = DispatchRuntime {
            self_inbox: &self_inbox,
            relay_information: &relay_information,
            runtime: rt.handle(),
            nip11_decisions: &nip11_decisions,
            auth_policies: &auth_policies,
            auth_tasks: &auth_tasks,
        };

        let effects = core.handle(EngineMsg::Subscribe(query, Box::new(NullRowSink)));
        let id = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitRows(id, ..) => Some(*id),
                _ => None,
            })
            .expect("initial protected target exists until preflight resolves");
        let (rows_tx, _rows_rx) = rows_channel();
        rows.insert(id, rows_tx);

        let mut opened = None;
        let mut attempts = 0usize;
        let refusal = preflight_query_relay_workers_with(
            &effects,
            |session| pool.live_session_handle(session).is_some(),
            |session| {
                attempts += 1;
                if opened.is_none() {
                    let handle = pool.ensure_session(session).unwrap();
                    opened = Some((session.clone(), handle));
                    Ok(Some(handle))
                } else {
                    Err(EngineThreadError::ThreadUnavailable {
                        component: "relay worker".to_string(),
                        reason: "injected protected subscribe refusal".to_string(),
                    })
                }
            },
            |handle| {
                let _ = pool.close(handle);
            },
        )
        .unwrap_err();
        assert_eq!(attempts, 2, "both deduplicated sessions were preflighted");
        assert!(matches!(
            refusal,
            EngineThreadError::ThreadUnavailable { component, reason }
                if component == "relay worker"
                    && reason == "injected protected subscribe refusal"
        ));
        // The landed preflight rolls back every worker it just opened when a
        // later session's spawn is refused (same discipline as the history
        // preflight): the refused subscribe leaves no live protected worker
        // behind.
        assert!(pool
            .live_session_handle(&opened.as_ref().unwrap().0)
            .is_none());

        rows.remove(&id);
        let withdraw = core.handle(EngineMsg::Unsubscribe(id));
        dispatch_core_effects(
            &mut core,
            withdraw,
            &pool,
            &mut rows,
            &mut histories,
            &mut diagnostics,
            &mut preambles,
            &registry,
            dispatch_runtime,
        );

        assert!(!rows.contains_key(&id));
        assert_eq!(core.required_relay_workers(), Some(BTreeSet::new()));
        assert!(pool.live_session_handle(&first_session).is_none());
        assert!(pool.live_session_handle(&refused_session).is_none());
        assert!(preambles.is_empty());
        let snapshot = core.diagnostics_snapshot();
        assert!(snapshot.relays.is_empty());
        assert!(snapshot.auth_sessions.is_empty());

        let late = core.handle(EngineMsg::RelayOpenFailed(
            refused_session.clone(),
            "late unowned failure".to_string(),
        ));
        assert!(late.is_empty());
        assert!(core.diagnostics_snapshot().transport_degraded.is_none());

        assert!(
            preflight_query_relay_workers_with(&effects, |_| false, |_| Ok(None), |_| {}).is_ok(),
            "capacity refusal remains ordinary local shortfall"
        );
        let duplicate_edges = [
            Effect::EnsureRelay(first_session.clone()),
            Effect::EnsureRelay(first_session.clone()),
        ];
        let mut duplicate_attempts = 0usize;
        preflight_query_relay_workers_with(
            &duplicate_edges,
            |_| false,
            |_| {
                duplicate_attempts += 1;
                Ok(None)
            },
            |_| {},
        )
        .unwrap();
        assert_eq!(duplicate_attempts, 1, "EnsureRelay edges are deduplicated");

        pool.shutdown();
    }

    #[test]
    fn relay_open_failure_clears_after_retry_connect_and_after_withdrawal() {
        let signer = Keys::generate().public_key();
        let relay = RelayUrl::parse("ws://127.0.0.1:9").unwrap();
        let session = RelaySessionKey::new(relay.clone(), AccessContext::Nip42(signer));
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 1);
        let (pool_tx, _pool_rx) = mpsc::channel();
        let mut config = PoolConfig::default();
        config.max_relays = 1;
        let pool = Pool::new(config, pool_tx).expect("test pool construction");
        let mut rows = HashMap::new();
        let mut histories = HashMap::new();
        let mut diagnostics = HashMap::new();
        let mut preambles = Preambles::new();
        let registry = SignerRegistry::default();
        let (self_inbox, inbox_rx) = mpsc::channel();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let relay_information = RelayInformationService::new(rt.handle().clone());
        let nip11_decisions = RefCell::new(Nip11DecisionState::default());
        let auth_policies = RefCell::new(auth::AuthPolicyRegistry::default());
        let auth_tasks = RefCell::new(auth::AuthTaskRegistry::default());
        let dispatch_runtime = DispatchRuntime {
            self_inbox: &self_inbox,
            relay_information: &relay_information,
            runtime: rt.handle(),
            nip11_decisions: &nip11_decisions,
            auth_policies: &auth_policies,
            auth_tasks: &auth_tasks,
        };

        let effects = core.handle(EngineMsg::Subscribe(
            protected_query(&relay, signer, 1),
            Box::new(NullRowSink),
        ));
        let id = effects
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitRows(id, ..) => Some(*id),
                _ => None,
            })
            .expect("protected subscription handle");

        dispatch_relay_open_failure(
            &mut core,
            session.clone(),
            nmp_transport::RelayOpenError::ThreadUnavailable(nmp_transport::ThreadSpawnError {
                role: nmp_transport::ThreadRole::RelayWorker,
                reason: "injected retryable open failure".to_string(),
            }),
            &pool,
            &mut rows,
            &mut histories,
            &mut diagnostics,
            &mut preambles,
            &registry,
            dispatch_runtime,
        );
        assert!(core
            .diagnostics_snapshot()
            .transport_degraded
            .as_deref()
            .is_some_and(|failure| failure.contains("injected retryable open failure")));
        assert!(matches!(
            inbox_rx.recv_timeout(Duration::from_secs(1)),
            Ok(Cmd::RelayWorkerRetired)
        ));
        assert!(
            inbox_rx.try_recv().is_err(),
            "one refusal creates exactly one retry edge"
        );

        retry_required_relay_workers(&core, &pool, &mut preambles);
        let handle = pool
            .live_session_handle(&session)
            .expect("bounded retry opens the still-owned session");
        core.handle(EngineMsg::RelayConnected(handle, session.clone()));
        assert!(core.diagnostics_snapshot().transport_degraded.is_none());

        core.handle(EngineMsg::RelayOpenFailed(
            session.clone(),
            "withdraw-owned".to_string(),
        ));
        assert!(core
            .diagnostics_snapshot()
            .transport_degraded
            .as_deref()
            .is_some_and(|failure| failure.contains("withdraw-owned")));
        let withdraw = core.handle(EngineMsg::Unsubscribe(id));
        assert!(core.diagnostics_snapshot().transport_degraded.is_none());
        assert_eq!(core.required_relay_workers(), Some(BTreeSet::new()));
        dispatch_core_effects(
            &mut core,
            withdraw,
            &pool,
            &mut rows,
            &mut histories,
            &mut diagnostics,
            &mut preambles,
            &registry,
            dispatch_runtime,
        );

        pool.shutdown();
    }

    #[test]
    fn repeated_engine_shutdown_returns_runtime_threads_to_exact_baseline() {
        let _serial = RUNTIME_LIFECYCLE_TEST_LOCK.lock().unwrap();
        for _ in 0..16 {
            let (engine, handle) = EngineThread::spawn(
                MemoryStore::new(),
                FixtureDirectory::new(),
                1,
                PoolConfig::default(),
                RelayAdmissionPolicy::default(),
            )
            .expect("engine construction");
            let runtime_threads = Arc::clone(&engine.runtime_threads);
            let deadline = Instant::now() + Duration::from_secs(5);
            // #704: the auth-release bridge is gone (the adapter executor was
            // replaced by the tokio runtime, whose workers are NOT counted by
            // this reducer/bridge guard). One engine now owns exactly the
            // reducer thread + the pool-bridge thread.
            while runtime_threads.load(std::sync::atomic::Ordering::SeqCst) != 2
                && Instant::now() < deadline
            {
                thread::yield_now();
            }
            assert_eq!(
                runtime_threads.load(std::sync::atomic::Ordering::SeqCst),
                2,
                "one engine must own exactly its reducer and pool-bridge threads"
            );
            handle.shutdown();
            engine.join();
            assert_eq!(
                runtime_threads.load(std::sync::atomic::Ordering::SeqCst),
                0,
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
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let relay_information = RelayInformationService::new(rt.handle().clone());
        let nip11_decisions = RefCell::new(Nip11DecisionState::default());
        let auth_policies = RefCell::new(auth::AuthPolicyRegistry::default());
        let auth_tasks = RefCell::new(auth::AuthTaskRegistry::default());
        let dispatch_runtime = DispatchRuntime {
            self_inbox: &self_inbox,
            relay_information: &relay_information,
            runtime: rt.handle(),
            nip11_decisions: &nip11_decisions,
            auth_policies: &auth_policies,
            auth_tasks: &auth_tasks,
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
            &mut core,
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
            &mut core,
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
            &mut core,
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
    }

    /// Exact read reconciliation must not evict a worker owned only by a
    /// durable write lane. A socket is shared transport state: releasing it
    /// from the router plan is safe only after every nonterminal outbox lane
    /// for that relay is also gone.
    #[test]
    fn durable_write_lane_retains_worker_without_read_demand() {
        let author = Keys::generate();
        let relay = RelayUrl::parse("wss://write-only.example").unwrap();
        // With the #8 AUTH reducer landed, the write plane rides the signing
        // identity's authenticated session, so the worker this durable lane
        // owns is the Nip42 session for (relay, author).
        let write_session = RelaySessionKey::new(
            relay.clone(),
            nmp_grammar::AccessContext::Nip42(author.public_key()),
        );
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
                identity_override: None,
                correlation: None,
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
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let relay_information = RelayInformationService::new(rt.handle().clone());
        let nip11_decisions = RefCell::new(Nip11DecisionState::default());
        let auth_policies = RefCell::new(auth::AuthPolicyRegistry::default());
        let auth_tasks = RefCell::new(auth::AuthTaskRegistry::default());
        let dispatch_runtime = DispatchRuntime {
            self_inbox: &self_inbox,
            relay_information: &relay_information,
            runtime: rt.handle(),
            nip11_decisions: &nip11_decisions,
            auth_policies: &auth_policies,
            auth_tasks: &auth_tasks,
        };

        dispatch_core_effects(
            &mut core,
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
            &mut core,
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
    }
}

#[cfg(test)]
mod auth_registry_admission_tests {
    use super::*;
    use nmp_router::FixtureDirectory;
    use nmp_signer::LocalKeySigner;
    use nmp_store::MemoryStore;
    use nmp_transport::RelayFrame;
    use nostr::{Keys, RelayMessage};
    use std::borrow::Cow;
    use std::sync::Mutex;

    struct AllowPolicy;

    impl AuthPolicy for AllowPolicy {
        fn evaluate(&self, _request: AuthPolicyRequest) -> AuthPolicyOp {
            AuthPolicyOp::allow()
        }
    }

    struct OnePolicy {
        invoked: Sender<()>,
        operation: Mutex<Option<AuthPolicyOp>>,
    }

    impl AuthPolicy for OnePolicy {
        fn evaluate(&self, _request: AuthPolicyRequest) -> AuthPolicyOp {
            let _ = self.invoked.send(());
            self.operation
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .take()
                .expect("one policy operation")
        }
    }

    struct OneSigner {
        public_key: PublicKey,
        invoked: Sender<()>,
        operation: Mutex<Option<SignerOp<SignedEvent>>>,
    }

    struct TimestampSigner {
        keys: Keys,
        observed: Sender<Timestamp>,
    }

    impl SigningCapability for TimestampSigner {
        fn public_key(&self) -> Option<PublicKey> {
            Some(self.keys.public_key())
        }

        fn sign(&self, unsigned: UnsignedEvent) -> SignerOp<SignedEvent> {
            let _ = self.observed.send(unsigned.created_at);
            SignerOp::Ready(Err(nmp_signer::SignerError::Unavailable))
        }
    }

    impl SigningCapability for OneSigner {
        fn public_key(&self) -> Option<PublicKey> {
            Some(self.public_key)
        }

        fn sign(&self, _unsigned: UnsignedEvent) -> SignerOp<SignedEvent> {
            let _ = self.invoked.send(());
            self.operation
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .take()
                .expect("one signer operation")
        }
    }

    fn runtime(limit: usize) -> (EngineThread, Handle) {
        EngineThread::spawn_with_runtime_config(
            MemoryStore::new(),
            FixtureDirectory::new(),
            1,
            PoolConfig::default(),
            RelayAdmissionPolicy::default(),
            RuntimeConfig {
                max_auth_capabilities: limit,
            },
        )
        .unwrap()
    }

    #[test]
    fn unique_key_flood_is_finite_but_replacement_and_removal_reuse_capacity() {
        let (engine, handle) = runtime(1);
        let first = Keys::generate().public_key();
        let second = Keys::generate().public_key();
        let stale = handle.add_auth_policy(first, AllowPolicy).unwrap();
        assert!(!format!("{stale:?}").contains("instance"));
        let replacement = handle
            .add_auth_policy(first, AllowPolicy)
            .expect("same-key replacement consumes no additional capacity");
        assert_eq!(
            handle.add_auth_policy(second, AllowPolicy),
            Err(AddAuthPolicyError::RegistryFull { limit: 1 })
        );
        assert!(!handle.remove_auth_policy(stale));
        assert!(handle.remove_auth_policy(replacement));
        handle
            .add_auth_policy(second, AllowPolicy)
            .expect("exact removal releases capacity");
        handle.shutdown();
        engine.join();
    }

    #[test]
    fn first_read_only_auth_frame_advances_the_frozen_template_clock() {
        let (engine, handle) = runtime(2);
        let keys = Keys::generate();
        let relay = RelayUrl::parse("ws://127.0.0.1:9").unwrap();
        let session =
            RelaySessionKey::new(relay, nmp_grammar::AccessContext::Nip42(keys.public_key()));
        let transport = nmp_transport::RelayHandle {
            slot: 23,
            generation: 1,
        };
        let (observed_tx, observed_rx) = mpsc::channel();
        let signer = handle
            .add_signer(TimestampSigner {
                keys: keys.clone(),
                observed: observed_tx,
            })
            .unwrap();
        assert!(!format!("{signer:?}").contains("instance"));
        let policy = handle
            .add_auth_policy(keys.public_key(), AllowPolicy)
            .unwrap();
        handle
            .inbox
            .send(Cmd::Engine(EngineMsg::RelayConnected(
                transport,
                session.clone(),
            )))
            .unwrap();
        let before = Timestamp::now();
        handle
            .inbox
            .send(Cmd::Engine(EngineMsg::RelayFrame(
                transport,
                session,
                RelayFrame::from_message(RelayMessage::Auth {
                    challenge: Cow::Borrowed("first-frame-clock"),
                }),
            )))
            .unwrap();
        let created_at = observed_rx.recv().unwrap();
        assert!(created_at.as_secs() > 0);
        assert!(created_at >= before);
        assert!(handle.remove_signer(signer));
        assert!(handle.remove_auth_policy(policy));
        handle.shutdown();
        engine.join();
    }

    #[test]
    fn shutdown_drain_rejects_cancel_hook_handle_reentry_and_releases_executor() {
        let (engine, handle) = runtime(2);
        let keys = Keys::generate();
        let relay = RelayUrl::parse("ws://127.0.0.1:9").unwrap();
        let session =
            RelaySessionKey::new(relay, nmp_grammar::AccessContext::Nip42(keys.public_key()));
        let transport = nmp_transport::RelayHandle {
            slot: 29,
            generation: 1,
        };
        let hook_handle = handle.clone();
        let hook_key = Keys::generate().public_key();
        let (hook_result_tx, hook_result_rx) = mpsc::channel();
        let (_pending_sender, operation) = AuthPolicyOp::pending_channel_with_cancel(move || {
            let result = hook_handle
                .add_auth_policy(hook_key, AllowPolicy)
                .map(|_| ());
            let _ = hook_result_tx.send(result);
        });
        let (invoked_tx, invoked_rx) = mpsc::channel();
        handle
            .add_auth_policy(
                keys.public_key(),
                OnePolicy {
                    invoked: invoked_tx,
                    operation: Mutex::new(Some(operation)),
                },
            )
            .unwrap();
        handle
            .inbox
            .send(Cmd::Engine(EngineMsg::RelayConnected(
                transport,
                session.clone(),
            )))
            .unwrap();
        handle
            .inbox
            .send(Cmd::Engine(EngineMsg::RelayFrame(
                transport,
                session,
                RelayFrame::from_message(RelayMessage::Auth {
                    challenge: Cow::Borrowed("shutdown-reentry"),
                }),
            )))
            .unwrap();
        invoked_rx.recv().unwrap();
        handle.shutdown();
        engine.join();
        assert_eq!(
            hook_result_rx.recv().unwrap(),
            Err(AddAuthPolicyError::EngineShuttingDown)
        );
    }

    #[test]
    fn zero_auth_capacity_admits_none() {
        let (engine, handle) = runtime(0);
        let key = Keys::generate().public_key();
        assert_eq!(
            handle.add_auth_policy(key, AllowPolicy),
            Err(AddAuthPolicyError::RegistryFull { limit: 0 })
        );
        handle.shutdown();
        engine.join();
    }

    #[test]
    fn bound_policy_and_signer_removal_are_synchronous_before_callbacks() {
        let (engine, handle) = runtime(2);
        let keys = Keys::generate();
        let relay = RelayUrl::parse("ws://127.0.0.1:9").unwrap();
        let session =
            RelaySessionKey::new(relay, nmp_grammar::AccessContext::Nip42(keys.public_key()));
        let transport = nmp_transport::RelayHandle {
            slot: 19,
            generation: 1,
        };
        let (policy_invoked_tx, policy_invoked_rx) = mpsc::channel();
        let (_policy_sender, policy_op) = AuthPolicyOp::pending_channel();
        let policy = handle
            .add_auth_policy(
                keys.public_key(),
                OnePolicy {
                    invoked: policy_invoked_tx,
                    operation: Mutex::new(Some(policy_op)),
                },
            )
            .unwrap();
        handle
            .inbox
            .send(Cmd::Engine(EngineMsg::RelayConnected(
                transport,
                session.clone(),
            )))
            .unwrap();
        handle
            .inbox
            .send(Cmd::Engine(EngineMsg::RelayFrame(
                transport,
                session.clone(),
                RelayFrame::from_message(RelayMessage::Auth {
                    challenge: Cow::Borrowed("policy-race"),
                }),
            )))
            .unwrap();
        policy_invoked_rx.recv().unwrap();
        assert!(handle.remove_auth_policy(policy));
        let (diagnostics, snapshots) = handle.observe_diagnostics();
        let snapshot = snapshots.recv().unwrap();
        let auth = snapshot
            .auth_sessions
            .iter()
            .find(|auth| auth.relay == session.relay)
            .unwrap();
        assert!(auth.policy_bound);
        assert_eq!(auth.phase, crate::core::AuthDiagnosticsPhase::Error);
        drop(diagnostics);

        let signer = LocalKeySigner::new(keys.clone());
        let signer_registration = handle.add_signer(signer).unwrap();
        let policy_registration = handle
            .add_auth_policy(keys.public_key(), AllowPolicy)
            .unwrap();
        let (signer_invoked_tx, signer_invoked_rx) = mpsc::channel();
        let (_signer_sender, signer_op) = SignerOp::pending_channel();
        assert!(handle.remove_signer(signer_registration));
        let signer_registration = handle
            .add_signer(OneSigner {
                public_key: keys.public_key(),
                invoked: signer_invoked_tx,
                operation: Mutex::new(Some(signer_op)),
            })
            .unwrap();
        handle
            .inbox
            .send(Cmd::Engine(EngineMsg::RelayFrame(
                transport,
                session.clone(),
                RelayFrame::from_message(RelayMessage::Auth {
                    challenge: Cow::Borrowed("signer-race"),
                }),
            )))
            .unwrap();
        signer_invoked_rx.recv().unwrap();
        assert!(handle.remove_signer(signer_registration));
        let (_diagnostics, snapshots) = handle.observe_diagnostics();
        let snapshot = snapshots.recv().unwrap();
        let auth = snapshot
            .auth_sessions
            .iter()
            .find(|auth| auth.relay == session.relay)
            .unwrap();
        assert!(auth.signer_bound);
        assert_eq!(auth.phase, crate::core::AuthDiagnosticsPhase::Error);
        assert!(handle.remove_auth_policy(policy_registration));
        handle.shutdown();
        engine.join();
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
        PoolEvent::InitialReadCompleted { handle, session } => {
            Some(EngineMsg::AuthProbeReleased(handle, session))
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

#[cfg(test)]
fn relay_frame_is_auth(frame: &RelayFrame) -> bool {
    matches!(
        frame,
        RelayFrame::Message(message)
            if matches!(message.as_ref(), RelayMessage::Auth { .. })
    )
}

/// Frames whose reducer handling consumes wall-clock truth. EOSE and
/// NEG-MSG may mint coverage; AUTH creates timestamped challenge state.
/// Advance the pure reducer clock immediately before the batch containing
/// them so coverage `through` is completion-time capped and can never be
/// influenced by an EVENT's `created_at`.
fn relay_frame_needs_wall_clock(frame: &RelayFrame) -> bool {
    matches!(
        frame,
        RelayFrame::Message(message)
            if matches!(
                message.as_ref(),
                RelayMessage::Auth { .. }
                    | RelayMessage::EndOfStoredEvents(_)
                    | RelayMessage::NegMsg { .. }
            )
    )
}

/// Per-SESSION reconnect-preamble bookkeeping: the full set of currently-live
/// REQ wire texts, keyed by `SubId` so `WireOp::Req`/`Close` can update it
/// incrementally (module doc: `Pool::set_reconnect_preamble` replaces the
/// WHOLE preamble on every call, so this module must always hand it the
/// complete current set, not a delta). PROTECTED sessions never own an entry
/// here (#8): their REQs must never auto-replay on reconnect — a fresh
/// generation is unauthenticated until its own AUTH completes, and the
/// engine re-issues `Effect::Replay` itself when the AUTH reducer reaches
/// Ready for that exact generation (`finish_auth_ok` in `core/mod.rs`).
type Preambles = HashMap<RelaySessionKey, HashMap<SubId, String>>;

#[derive(Clone, Copy)]
struct DispatchRuntime<'a> {
    self_inbox: &'a Sender<Cmd>,
    relay_information: &'a RelayInformationService,
    runtime: &'a tokio::runtime::Handle,
    nip11_decisions: &'a RefCell<Nip11DecisionState>,
    auth_policies: &'a RefCell<auth::AuthPolicyRegistry>,
    auth_tasks: &'a RefCell<auth::AuthTaskRegistry>,
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
/// The deadline-armed driver (§3.3, #39): every iteration re-reads the core
/// and NIP-11 decision deadlines, then waits for their exact minimum. A
/// command that introduces an earlier deadline re-arms naturally on the next
/// iteration; there is no polling or sleeper. `None` blocks on plain
/// `recv()`. A timeout fires only the due owners: reducer `Tick` for
/// persisted deadlines and/or NIP-11 fallback, then recomputes the minimum.
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
        runtime,
        relay_information,
        max_auth_capabilities,
    } = pool_runtime;
    let runtime_handle = runtime.handle().clone();
    let runtime_handle = &runtime_handle;
    let mut core = EngineCore::new(store, Box::new(directory), cap).with_relay_admission(admission);
    let mut row_channels: HashMap<HandleId, RowsSender> = HashMap::new();
    let mut history_channels: HashMap<HistorySessionId, LatestSender<HistoryMsg>> = HashMap::new();
    let mut diag_channels: HashMap<u64, LatestSender<DiagnosticsSnapshot>> = HashMap::new();
    let mut next_diag_id: u64 = 0;
    let mut preambles: Preambles = Preambles::new();
    let mut registry = SignerRegistry::default();
    let auth_policies = RefCell::new(auth::AuthPolicyRegistry::default());
    let auth_tasks = RefCell::new(auth::AuthTaskRegistry::default());
    let mut auth_instances = auth::AuthCapabilityInstances::default();
    let mut active_pubkey = None;
    let mut next_sign_event_id = 1u64;
    let mut sign_event_cancellations: HashMap<u64, ActiveSignEvent> = HashMap::new();
    let nip11_decisions = RefCell::new(Nip11DecisionState::default());
    let dispatch_runtime = DispatchRuntime {
        self_inbox,
        relay_information: &relay_information,
        runtime: runtime_handle,
        nip11_decisions: &nip11_decisions,
        auth_policies: &auth_policies,
        auth_tasks: &auth_tasks,
    };

    // Recovery happens before the first externally-issued command. Pending
    // rows already live in the store; this only rebuilds ownership and may
    // replay exact durable attempt bytes whose Started fact was committed.
    let recovery_effects = core.recover_on_boot();
    dispatch_core_effects(
        &mut core,
        recovery_effects,
        &pool,
        &mut row_channels,
        &mut history_channels,
        &mut diag_channels,
        &mut preambles,
        &registry,
        dispatch_runtime,
    );

    let mut shutting_down = false;
    loop {
        let core_wait = core
            .next_deadline()
            .map(|deadline| duration_until(deadline, Timestamp::now()));
        let nip11_wait = nip11_decisions
            .borrow()
            .next_deadline()
            .map(|deadline| deadline.saturating_duration_since(Instant::now()));
        let wait = if shutting_down {
            None
        } else {
            [core_wait, nip11_wait].into_iter().flatten().min()
        };
        let cmd = match wait {
            None => match cmd_rx.recv() {
                Ok(cmd) => cmd,
                Err(_) => break, // every `Sender` (incl. `self_inbox`) is gone.
            },
            Some(wait) => match cmd_rx.recv_timeout(wait) {
                Ok(cmd) => cmd,
                Err(RecvTimeoutError::Timeout) => {
                    // Core deadlines and NIP-11 fallback share this one
                    // event-driven wait. Fire only the owners actually due,
                    // then re-arm the exact minimum.
                    for url in nip11_decisions
                        .borrow_mut()
                        .take_due_fallbacks(Instant::now())
                    {
                        let effects = core.handle(EngineMsg::RelayInformationResolved(url, None));
                        dispatch_core_effects(
                            &mut core,
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
                            &mut core,
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
        if shutting_down {
            match cmd {
                Cmd::AuthTaskReleased(release) => {
                    let _ = auth_tasks.borrow_mut().released(release);
                }
                Cmd::AuthTaskCompleted(completion) => {
                    let _ = auth_tasks.borrow_mut().finish(completion);
                }
                Cmd::AddAuthPolicy { reply, .. } => {
                    let _ = reply.send(Err(AddAuthPolicyError::EngineShuttingDown));
                }
                Cmd::AddSigner { reply, .. } => {
                    let _ = reply.send(Err(AddSignerError::EngineShuttingDown));
                }
                Cmd::Subscribe { reply, .. } => {
                    let _ = reply.send(Err(EngineThreadError::EngineShuttingDown));
                }
                Cmd::SubscribeHistory { reply, .. } => {
                    let _ = reply.send(Err(EngineThreadError::EngineShuttingDown));
                }
                Cmd::RequestRows { reply, .. } => {
                    let _ = reply.send(Err(HistoryAdvanceError::TransportUnavailable {
                        reason: "engine is shutting down".to_string(),
                    }));
                }
                Cmd::PublishTracked { reply, .. } => {
                    let _ = reply.send(Err(PublishError::EngineShuttingDown));
                }
                Cmd::CancelWrite { reply, .. } => {
                    let _ = reply.send(Err(CancelWriteError::EngineClosed));
                }
                Cmd::SignEvent { reply, .. } => {
                    let _ = reply.send(Err(SignEventError::EngineClosed));
                }
                Cmd::RemoveAuthPolicy {
                    registration,
                    reply,
                } => {
                    let removed = auth_policies.borrow_mut().remove(&registration).is_some();
                    let _ = reply.send(removed);
                }
                Cmd::RemoveSigner {
                    registration,
                    reply,
                } => {
                    let removed = registry.remove(&registration).is_some();
                    let _ = reply.send(removed);
                }
                Cmd::ReattachReceipt {
                    id,
                    cursor,
                    sink,
                    registration,
                    reply,
                } => {
                    let _ = reply.send(core.reattach_receipt_page_registered(
                        id,
                        sink,
                        cursor,
                        FACT_CHANNEL_CAPACITY,
                        Some(registration),
                    ));
                }
                Cmd::ReattachByCorrelation {
                    token,
                    sink,
                    registration,
                    reply,
                } => {
                    let _ = reply.send(core.reattach_by_correlation_page_registered(
                        token,
                        sink,
                        None,
                        FACT_CHANNEL_CAPACITY,
                        Some(registration),
                    ));
                }
                Cmd::DetachReceiptSink { id, registration } => {
                    core.detach_receipt_sink(id, &registration);
                }
                #[cfg(test)]
                Cmd::ReceiptSinkCount { id, reply } => {
                    let _ = reply.send(core.receipt_sink_count(id));
                }
                Cmd::ObserveDiagnostics { reply } => {
                    let id = next_diag_id;
                    next_diag_id = next_diag_id.saturating_add(1);
                    let (tx, rx) = latest_channel();
                    tx.send(core.diagnostics_snapshot());
                    if reply.send((id, rx)).is_ok() {
                        diag_channels.insert(id, tx);
                    }
                }
                Cmd::RelayBatch { applied, .. } => {
                    let _ = applied.send(());
                }
                Cmd::CancelSignEvent(id) | Cmd::SignEventFinished(id) => {
                    if let Some(active) = sign_event_cancellations.remove(&id) {
                        active.terminal.cancel();
                    }
                }
                Cmd::ExemptSignEventDrain(op_id) => {
                    sign_event_cancellations.remove(&op_id);
                }
                Cmd::Engine(_)
                | Cmd::RelayInformationFetched { .. }
                | Cmd::RelayWorkerRetired
                | Cmd::UnobserveDiagnostics(_)
                | Cmd::UnsubscribeHistory(_)
                | Cmd::Shutdown => {}
            }
            if auth_tasks.borrow().is_empty() && sign_event_cancellations.is_empty() {
                break;
            }
            continue;
        }
        match cmd {
            Cmd::Shutdown => {
                shutting_down = true;
                auth_tasks.borrow_mut().shutdown();
                registry.cancel_all_pending_writes();
                for active in sign_event_cancellations.values() {
                    active.terminal.cancel();
                }
                if auth_tasks.borrow().is_empty() && sign_event_cancellations.is_empty() {
                    break;
                }
            }
            Cmd::ExemptSignEventDrain(op_id) => {
                sign_event_cancellations.remove(&op_id);
            }
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
                    &mut core,
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
                #[cfg(feature = "bench-instrumentation")]
                let batch_started = std::time::Instant::now();
                if frames.iter().any(|(handle, session, frame)| {
                    relay_frame_needs_wall_clock(frame)
                        && core.is_current_transport_session(*handle, session)
                }) {
                    let tick_effects = core.handle(EngineMsg::Tick(Timestamp::now()));
                    dispatch_core_effects(
                        &mut core,
                        tick_effects,
                        &pool,
                        &mut row_channels,
                        &mut history_channels,
                        &mut diag_channels,
                        &mut preambles,
                        &registry,
                        dispatch_runtime,
                    );
                }
                let mut ordinary = Vec::new();
                let mut committed = Vec::new();
                for (handle, session, frame) in frames {
                    match frame {
                        RelayFrame::CommittedObservation(hit) => {
                            if !ordinary.is_empty() {
                                reduce_and_dispatch_relay_frames(
                                    &mut core,
                                    std::mem::take(&mut ordinary),
                                    &pool,
                                    &mut row_channels,
                                    &mut history_channels,
                                    &mut diag_channels,
                                    &mut preambles,
                                    &registry,
                                    dispatch_runtime,
                                );
                            }
                            let valid = core.is_current_transport_session(handle, &session)
                                && !core.committed_observation_conflicts_with_pending(&hit);
                            if valid {
                                committed.push((
                                    handle,
                                    session,
                                    RelayFrame::CommittedObservation(hit),
                                ));
                            } else {
                                if !committed.is_empty() {
                                    reduce_and_dispatch_committed_observations(
                                        &mut core,
                                        std::mem::take(&mut committed),
                                        &pool,
                                        &mut row_channels,
                                        &mut history_channels,
                                        &mut diag_channels,
                                        &mut preambles,
                                        &registry,
                                        dispatch_runtime,
                                    );
                                }
                                if let Some(frame) =
                                    RelayFrame::CommittedObservation(hit).into_ordinary_fallback()
                                {
                                    ordinary.push((handle, session, frame));
                                }
                            }
                        }
                        frame => {
                            if !committed.is_empty() {
                                reduce_and_dispatch_committed_observations(
                                    &mut core,
                                    std::mem::take(&mut committed),
                                    &pool,
                                    &mut row_channels,
                                    &mut history_channels,
                                    &mut diag_channels,
                                    &mut preambles,
                                    &registry,
                                    dispatch_runtime,
                                );
                            }
                            ordinary.push((handle, session, frame));
                        }
                    }
                }
                if !committed.is_empty() {
                    reduce_and_dispatch_committed_observations(
                        &mut core,
                        committed,
                        &pool,
                        &mut row_channels,
                        &mut history_channels,
                        &mut diag_channels,
                        &mut preambles,
                        &registry,
                        dispatch_runtime,
                    );
                }
                if !ordinary.is_empty() {
                    reduce_and_dispatch_relay_frames(
                        &mut core,
                        ordinary,
                        &pool,
                        &mut row_channels,
                        &mut history_channels,
                        &mut diag_channels,
                        &mut preambles,
                        &registry,
                        dispatch_runtime,
                    );
                }
                #[cfg(feature = "bench-instrumentation")]
                crate::ingest_attribution::engine_batch_process(batch_started.elapsed());
                let _ = applied.send(());
            }
            Cmd::AddSigner { signer, reply } => {
                let result = signer
                    .public_key()
                    .ok_or(AddSignerError::MissingPublicKey)
                    .and_then(|pubkey| {
                        let live = registry.len().saturating_add(auth_policies.borrow().len());
                        if !registry.contains(pubkey) && live >= max_auth_capabilities {
                            return Err(AddSignerError::RegistryFull {
                                limit: max_auth_capabilities,
                            });
                        }
                        let instance = auth_instances
                            .mint()
                            .ok_or(AddSignerError::CapabilityInstanceExhausted)?;
                        Ok(registry.add(pubkey, instance, signer))
                    });
                match result {
                    Ok((registration, replaced)) => {
                        let mut effects = Vec::new();
                        if let Some(instance) = replaced {
                            auth_tasks.borrow_mut().cancel_capability(
                                registration.public_key(),
                                core::AuthCapability::Signer,
                                instance,
                            );
                            effects.extend(core.handle(EngineMsg::AuthCapabilityInvalidated(
                                registration.public_key(),
                                core::AuthCapability::Signer,
                                instance,
                            )));
                        }
                        effects.extend(
                            core.handle(EngineMsg::SignerAttached(registration.public_key())),
                        );
                        dispatch_core_effects(
                            &mut core,
                            effects,
                            &pool,
                            &mut row_channels,
                            &mut history_channels,
                            &mut diag_channels,
                            &mut preambles,
                            &registry,
                            dispatch_runtime,
                        );
                        let _ = reply.send(Ok(registration));
                    }
                    Err(error) => {
                        let _ = reply.send(Err(error));
                    }
                }
            }
            Cmd::RemoveSigner {
                registration,
                reply,
            } => {
                let removed = registry.remove(&registration);
                if let Some(instance) = removed {
                    auth_tasks.borrow_mut().cancel_capability(
                        registration.public_key(),
                        core::AuthCapability::Signer,
                        instance,
                    );
                    let effects = core.handle(EngineMsg::AuthCapabilityInvalidated(
                        registration.public_key(),
                        core::AuthCapability::Signer,
                        instance,
                    ));
                    dispatch_core_effects(
                        &mut core,
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
                let _ = reply.send(removed.is_some());
            }
            Cmd::AddAuthPolicy {
                expected_pubkey,
                policy,
                reply,
            } => {
                let live = registry.len().saturating_add(auth_policies.borrow().len());
                if !auth_policies.borrow().contains(expected_pubkey)
                    && live >= max_auth_capabilities
                {
                    let _ = reply.send(Err(AddAuthPolicyError::RegistryFull {
                        limit: max_auth_capabilities,
                    }));
                    continue;
                }
                let Some(instance) = auth_instances.mint() else {
                    let _ = reply.send(Err(AddAuthPolicyError::CapabilityInstanceExhausted));
                    continue;
                };
                let (registration, replaced) =
                    auth_policies
                        .borrow_mut()
                        .add(expected_pubkey, instance, policy);
                if let Some(old_instance) = replaced {
                    auth_tasks.borrow_mut().cancel_capability(
                        expected_pubkey,
                        core::AuthCapability::Policy,
                        old_instance,
                    );
                    let effects = core.handle(EngineMsg::AuthCapabilityInvalidated(
                        expected_pubkey,
                        core::AuthCapability::Policy,
                        old_instance,
                    ));
                    dispatch_core_effects(
                        &mut core,
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
                let _ = reply.send(Ok(registration));
            }
            Cmd::RemoveAuthPolicy {
                registration,
                reply,
            } => {
                let removed = auth_policies.borrow_mut().remove(&registration);
                if let Some(instance) = removed {
                    auth_tasks.borrow_mut().cancel_capability(
                        registration.expected_pubkey(),
                        core::AuthCapability::Policy,
                        instance,
                    );
                    let effects = core.handle(EngineMsg::AuthCapabilityInvalidated(
                        registration.expected_pubkey(),
                        core::AuthCapability::Policy,
                        instance,
                    ));
                    dispatch_core_effects(
                        &mut core,
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
                let _ = reply.send(removed.is_some());
            }
            Cmd::AuthTaskCompleted(completion) => {
                let Some(msg) = auth_tasks.borrow_mut().finish(completion) else {
                    continue;
                };
                let effects = core.handle(msg);
                dispatch_core_effects(
                    &mut core,
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
            Cmd::AuthTaskReleased(release) => {
                let pending = auth_tasks.borrow_mut().released(release);
                if let Some(task) = pending {
                    auth::launch_auth_task(
                        task,
                        &mut auth_tasks.borrow_mut(),
                        runtime_handle,
                        self_inbox,
                    );
                }
            }
            Cmd::SignEvent {
                unsigned,
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
                let Some(signer_op) = registry.sign(unsigned.clone()) else {
                    let _ = reply.send(Err(SignEventError::NoActiveSigner));
                    continue;
                };

                let operation_id = next_sign_event_id;
                next_sign_event_id = next_sign_event_id.wrapping_add(1).max(1);

                // #704: the SIGNING WAIT holds no thread. A ready local signer
                // has its result now; a pending remote signer is awaited by an
                // async task on the adapter runtime. Cancellation fires the
                // pending op's canceller (a no-op for a ready result); the
                // foreign `completion` — which may block and may call
                // `Engine::join()` reentrantly — always runs on a FRESH per-op
                // OS thread, never the runtime or the reducer.
                let (cancel_action, signer_source): (
                    Box<dyn Fn() + Send + Sync>,
                    SignEventSignerResult,
                ) = match signer_op {
                    SignerOp::Ready(result) => (
                        Box::new(|| {}),
                        SignEventSignerResult::Ready(Box::new(result)),
                    ),
                    SignerOp::Pending(pending) => {
                        let canceller = pending.canceller();
                        (
                            Box::new(move || canceller.cancel()),
                            SignEventSignerResult::Pending(pending),
                        )
                    }
                };
                let terminal = SignEventTerminal::new(cancel_action);

                sign_event_cancellations.insert(
                    operation_id,
                    ActiveSignEvent {
                        terminal: Arc::clone(&terminal),
                    },
                );
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
                match signer_source {
                    SignEventSignerResult::Ready(result) => {
                        spawn_sign_event_completion(
                            inbox,
                            operation_id,
                            terminal,
                            unsigned,
                            expected_id,
                            Some(*result),
                            completion,
                        );
                    }
                    SignEventSignerResult::Pending(pending) => {
                        // The signing wait is async; the (possibly-blocking)
                        // foreign completion is delivered on a per-op thread
                        // whether the await resolves OR the task's future is
                        // dropped at runtime shutdown (the dispatch Drop guard).
                        let dispatch = SignEventCompletionDispatch {
                            inbox,
                            operation_id,
                            terminal,
                            unsigned,
                            expected_id,
                            completion: Some(completion),
                            signer_result: None,
                        };
                        runtime_handle.spawn(async move {
                            let mut dispatch = dispatch;
                            let result = pending.await;
                            dispatch.signer_result = Some(result);
                            // drop(dispatch) here spawns the completion thread.
                        });
                    }
                }
            }
            Cmd::CancelSignEvent(id) => {
                if let Some(active) = sign_event_cancellations.remove(&id) {
                    active.terminal.cancel();
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
            Cmd::ReattachReceipt {
                id,
                cursor,
                sink,
                registration,
                reply,
            } => {
                let found = core.reattach_receipt_page_registered(
                    id,
                    sink,
                    cursor,
                    FACT_CHANNEL_CAPACITY,
                    Some(registration),
                );
                let _ = reply.send(found);
            }
            Cmd::ReattachByCorrelation {
                token,
                sink,
                registration,
                reply,
            } => {
                let found = core.reattach_by_correlation_page_registered(
                    token,
                    sink,
                    None,
                    FACT_CHANNEL_CAPACITY,
                    Some(registration),
                );
                let _ = reply.send(found);
            }
            Cmd::DetachReceiptSink { id, registration } => {
                core.detach_receipt_sink(id, &registration);
            }
            #[cfg(test)]
            Cmd::ReceiptSinkCount { id, reply } => {
                let _ = reply.send(core.receipt_sink_count(id));
            }
            Cmd::CancelWrite { id, reply } => {
                let (result, effects) = core.cancel_write(id);
                if result == Ok(CancelWriteOutcome::Cancelled) {
                    registry.cancel_pending_write(id);
                }
                let _ = reply.send(result);
                dispatch_core_effects(
                    &mut core,
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
            Cmd::PublishTracked {
                intent,
                sink,
                registration,
                reply,
            } => {
                let mut effects = core.handle(EngineMsg::Tick(Timestamp::now()));
                let publish_effects = core.handle(EngineMsg::Publish(intent, sink));
                let result = publish_result(&publish_effects);
                if let Ok(id) = result {
                    // A write may have reached a terminal state synchronously
                    // (for example, an already-signed ephemeral handoff), in
                    // which case no live sink remains to register.
                    let _ = core.register_initial_receipt_sink(id, registration.clone());
                }
                if reply.send(result).is_err() {
                    if let Ok(id) = result {
                        core.detach_receipt_sink(id, &registration);
                    }
                }
                effects.extend(publish_effects);
                dispatch_core_effects(
                    &mut core,
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
                let mut effects = core.handle(EngineMsg::Tick(Timestamp::now()));
                effects.extend(core.handle(EngineMsg::Subscribe(query, Box::new(NullRowSink))));
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
                let (rows_tx, rows_rx) = rows_channel();
                row_channels.insert(id, rows_tx);
                if let Err(error) = preflight_query_relay_workers(&effects, &pool) {
                    row_channels.remove(&id);
                    let withdraw = core.handle(EngineMsg::Unsubscribe(id));
                    dispatch_core_effects(
                        &mut core,
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
                        &mut core,
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
                    &mut core,
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
                let mut effects = core.handle(EngineMsg::Tick(Timestamp::now()));
                effects.extend(core.handle(EngineMsg::SubscribeHistory(
                    query,
                    Box::new(NullHistorySink),
                )));
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
                        &mut core,
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
                        &mut core,
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
                        &mut core,
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
                    &mut core,
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
                            &mut core,
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
                            &mut core,
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
                                &mut core,
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
                                &mut core,
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
                            &mut core,
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
                    &mut core,
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
                    &mut core,
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
            Cmd::Engine(EngineMsg::RelayFrame(handle, session, frame)) => {
                if relay_frame_needs_wall_clock(&frame)
                    && core.is_current_transport_session(handle, &session)
                {
                    let tick_effects = core.handle(EngineMsg::Tick(Timestamp::now()));
                    dispatch_core_effects(
                        &mut core,
                        tick_effects,
                        &pool,
                        &mut row_channels,
                        &mut history_channels,
                        &mut diag_channels,
                        &mut preambles,
                        &registry,
                        dispatch_runtime,
                    );
                }
                let effects = core.handle(EngineMsg::RelayFrame(handle, session, frame));
                dispatch_core_effects(
                    &mut core,
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
            Cmd::Engine(EngineMsg::Unsubscribe(id)) => {
                let effects = core.handle(EngineMsg::Unsubscribe(id));
                // Drop the sender: the app's `Receiver` observes disconnect.
                row_channels.remove(&id);
                dispatch_core_effects(
                    &mut core,
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
                    &mut core,
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
                    &mut core,
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
            Cmd::Engine(EngineMsg::SignerCompleted(id, generation, result)) => {
                registry.finish_pending_write(id, generation);
                let effects = core.handle(EngineMsg::SignerCompleted(id, generation, result));
                dispatch_core_effects(
                    &mut core,
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
                    &mut core,
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

    auth_tasks.borrow_mut().shutdown();
    registry.cancel_all_pending_writes();
    for (_, active) in sign_event_cancellations.drain() {
        active.terminal.cancel();
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

#[allow(clippy::too_many_arguments)]
fn reduce_and_dispatch_committed_observations<S: EventStore>(
    core: &mut EngineCore<S>,
    frames: Vec<(nmp_transport::RelayHandle, RelaySessionKey, RelayFrame)>,
    pool: &Pool,
    row_channels: &mut HashMap<HandleId, RowsSender>,
    history_channels: &mut HashMap<HistorySessionId, LatestSender<HistoryMsg>>,
    diag_channels: &mut HashMap<u64, LatestSender<DiagnosticsSnapshot>>,
    preambles: &mut Preambles,
    registry: &SignerRegistry,
    runtime: DispatchRuntime<'_>,
) {
    let all_valid = frames
        .iter()
        .all(|(_, _, frame)| matches!(frame, RelayFrame::CommittedObservation(_)))
        && pool.revalidate_committed_observations(frames.iter().filter_map(|(_, _, frame)| {
            match frame {
                RelayFrame::CommittedObservation(hit) => Some(hit),
                _ => None,
            }
        }));
    if all_valid {
        let observations = frames
            .into_iter()
            .filter_map(|(_, session, frame)| match frame {
                RelayFrame::CommittedObservation(hit) => Some((session, hit.event_kind())),
                _ => None,
            })
            .collect();
        let effects = core.on_revalidated_committed_observations(observations);
        dispatch_core_effects(
            core,
            effects,
            pool,
            row_channels,
            history_channels,
            diag_channels,
            preambles,
            registry,
            runtime,
        );
    } else {
        let frames = frames
            .into_iter()
            .filter_map(|(handle, session, frame)| {
                frame
                    .into_ordinary_fallback()
                    .map(|frame| (handle, session, frame))
            })
            .collect();
        reduce_and_dispatch_relay_frames(
            core,
            frames,
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

#[allow(clippy::too_many_arguments)]
fn reduce_and_dispatch_relay_frames<S: EventStore>(
    core: &mut EngineCore<S>,
    frames: Vec<(nmp_transport::RelayHandle, RelaySessionKey, RelayFrame)>,
    pool: &Pool,
    row_channels: &mut HashMap<HandleId, RowsSender>,
    history_channels: &mut HashMap<HistorySessionId, LatestSender<HistoryMsg>>,
    diag_channels: &mut HashMap<u64, LatestSender<DiagnosticsSnapshot>>,
    preambles: &mut Preambles,
    registry: &SignerRegistry,
    runtime: DispatchRuntime<'_>,
) {
    #[cfg(feature = "bench-instrumentation")]
    let phase_started = std::time::Instant::now();
    #[cfg(feature = "bench-instrumentation")]
    let cpu_started = crate::ingest_attribution::thread_cpu_time_ns();
    let effects = core.handle(EngineMsg::RelayFrames(frames));
    #[cfg(feature = "bench-instrumentation")]
    crate::ingest_attribution::relay_core_reduce(phase_started.elapsed());
    #[cfg(feature = "bench-instrumentation")]
    crate::ingest_attribution::relay_core_reduce_cpu(
        crate::ingest_attribution::thread_cpu_time_ns().saturating_sub(cpu_started),
    );
    #[cfg(feature = "bench-instrumentation")]
    let phase_started = std::time::Instant::now();
    dispatch_core_effects(
        core,
        effects,
        pool,
        row_channels,
        history_channels,
        diag_channels,
        preambles,
        registry,
        runtime,
    );
    #[cfg(feature = "bench-instrumentation")]
    crate::ingest_attribution::relay_effect_dispatch(phase_started.elapsed());
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
    core: &mut EngineCore<S>,
    effects: Vec<Effect>,
    pool: &Pool,
    row_channels: &mut HashMap<HandleId, RowsSender>,
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
        core,
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
            // A PROTECTED session's REQs stay parked until AUTH readiness,
            // so its acquisition edge is `Effect::EnsureRelay`, never a
            // `WireOp::Req` (#8 U4): the worker must exist before the relay
            // can deliver the challenge that makes readiness possible, and a
            // spawn refusal for it is the same typed construction failure as
            // for an ordinary REQ session.
            Effect::EnsureRelay(session) => {
                sessions.insert(session.clone());
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

#[allow(clippy::too_many_arguments)]
fn dispatch_relay_open_failure(
    core: &mut EngineCore<impl EventStore>,
    session: RelaySessionKey,
    error: nmp_transport::RelayOpenError,
    pool: &Pool,
    row_channels: &mut HashMap<HandleId, RowsSender>,
    history_channels: &mut HashMap<HistorySessionId, LatestSender<HistoryMsg>>,
    diag_channels: &mut HashMap<u64, LatestSender<DiagnosticsSnapshot>>,
    preambles: &mut Preambles,
    registry: &SignerRegistry,
    runtime: DispatchRuntime<'_>,
) {
    match error {
        nmp_transport::RelayOpenError::AtCapacity { .. } => {
            dispatch_effect(
                core,
                Effect::EmitDiagnostics(core.diagnostics_snapshot()),
                pool,
                row_channels,
                history_channels,
                diag_channels,
                preambles,
                registry,
                runtime,
            );
        }
        nmp_transport::RelayOpenError::ThreadUnavailable(error) => {
            let followups = core.handle(EngineMsg::RelayOpenFailed(
                session,
                format!("{}: {}", error.role, error.reason),
            ));
            dispatch_effects(
                core,
                followups,
                pool,
                row_channels,
                history_channels,
                diag_channels,
                preambles,
                registry,
                runtime,
            );
            // One event-driven retry after an OS refusal. A repeated refusal
            // remains latched in diagnostics and the reducer's required set;
            // it never turns into a command spin.
            let _ = runtime.self_inbox.send(Cmd::RelayWorkerRetired);
        }
        nmp_transport::RelayOpenError::Unavailable => {
            let followups = core.handle(EngineMsg::RelayOpenFailed(
                session,
                "relay pool state unavailable".to_string(),
            ));
            dispatch_effects(
                core,
                followups,
                pool,
                row_channels,
                history_channels,
                diag_channels,
                preambles,
                registry,
                runtime,
            );
        }
        nmp_transport::RelayOpenError::ShuttingDown => {
            if runtime
                .self_inbox
                .send(Cmd::Engine(EngineMsg::RelayOpenFailed(
                    session,
                    "relay pool is shutting down".to_string(),
                )))
                .is_err()
            {
                // The engine inbox is already gone; there is no observer or
                // retry owner left to notify.
            }
        }
    }
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
/// ordinary Connected (and, for protected, the AUTH reducer's ready
/// transition on the exact AUTH OK) path advances
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
    core: &mut EngineCore<impl EventStore>,
    effects: Vec<Effect>,
    pool: &Pool,
    row_channels: &mut HashMap<HandleId, RowsSender>,
    history_channels: &mut HashMap<HistorySessionId, LatestSender<HistoryMsg>>,
    diag_channels: &mut HashMap<u64, LatestSender<DiagnosticsSnapshot>>,
    preambles: &mut Preambles,
    registry: &SignerRegistry,
    runtime: DispatchRuntime<'_>,
) {
    for effect in effects {
        dispatch_effect(
            core,
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
    core: &mut EngineCore<impl EventStore>,
    effect: Effect,
    pool: &Pool,
    row_channels: &mut HashMap<HandleId, RowsSender>,
    history_channels: &mut HashMap<HistorySessionId, LatestSender<HistoryMsg>>,
    diag_channels: &mut HashMap<u64, LatestSender<DiagnosticsSnapshot>>,
    preambles: &mut Preambles,
    registry: &SignerRegistry,
    runtime: DispatchRuntime<'_>,
) {
    match effect {
        Effect::UpdateCommittedObservations {
            invalidated,
            published,
        } => {
            #[cfg(feature = "bench-instrumentation")]
            let phase_started = std::time::Instant::now();
            pool.update_committed_observations(invalidated, published);
            #[cfg(feature = "bench-instrumentation")]
            crate::ingest_attribution::committed_observation_effect(phase_started.elapsed());
        }
        Effect::Wire(delta) => apply_wire_delta(&delta, pool, preambles),
        Effect::PreflightHistoryRelays(_) => {}
        Effect::Replay(session, reqs) => apply_replay(&session, reqs, pool, preambles),
        Effect::ReleaseInitialRead(handle) => {
            let _ = pool.release_initial_read(handle);
        }
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
            if let Err(error) = pool.ensure_session(&session) {
                dispatch_relay_open_failure(
                    core,
                    session,
                    error,
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
        // The signer frozen into this exact accepted template is looked up
        // by pubkey on every request. A later active-account switch cannot
        // redirect outstanding work. No matching registered signer is
        // NOT a terminal signer failure. The accepted pending row and
        // obligation stay alive as `AwaitingCapability`; only an explicit
        // denial/error from an attached signer compensates the write.
        Effect::RequestSign(id, generation, unsigned) => match registry.sign(unsigned) {
            Some(operation) => match operation {
                SignerOp::Ready(result) => {
                    let _ = runtime
                        .self_inbox
                        .send(Cmd::Engine(EngineMsg::SignerCompleted(
                            id, generation, result,
                        )));
                }
                SignerOp::Pending(pending) => {
                    // #704: the remote-signer round-trip is awaited by an async
                    // task on the adapter runtime — no OS thread is held while
                    // it is outstanding. Write-cancel / account-switch fires the
                    // op's canceller (tracked below); dropping the task's future
                    // at runtime shutdown also runs the op's Drop cancel hook.
                    let inbox = runtime.self_inbox.clone();
                    let canceller = pending.canceller();
                    registry.track_pending_write(
                        id,
                        generation,
                        Box::new(move || canceller.cancel()),
                    );
                    runtime.runtime.spawn(async move {
                        let result = pending.await;
                        let _ = inbox.send(Cmd::Engine(EngineMsg::SignerCompleted(
                            id, generation, result,
                        )));
                    });
                }
            },
            None => {
                let _ = runtime
                    .self_inbox
                    .send(Cmd::Engine(EngineMsg::SignerUnavailable(id, generation)));
            }
        },
        Effect::RelayAuth(effect) => {
            let mut bind = |token, capability, instance| {
                let effects = core.handle(EngineMsg::AuthCapabilityBound {
                    token,
                    capability,
                    instance,
                });
                debug_assert!(
                    effects.is_empty(),
                    "binding an AUTH capability is a synchronous state-only transition"
                );
            };
            auth::dispatch(
                effect,
                pool,
                registry,
                &runtime.auth_policies.borrow(),
                &mut runtime.auth_tasks.borrow_mut(),
                runtime.runtime,
                runtime.self_inbox,
                &mut bind,
            );
        }
        Effect::RearmSignerIfAvailable(pubkey) => {
            if registry.is_available(pubkey) {
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
                tx.send((rows, evidence));
            }
        }
        Effect::EmitHistory(id, batch) => {
            if let Some(tx) = history_channels.get(&id) {
                #[cfg(feature = "bench-instrumentation")]
                let send_started = std::time::Instant::now();
                tx.send(batch);
                #[cfg(feature = "bench-instrumentation")]
                crate::ingest_attribution::history_channel_send(send_started.elapsed());
            }
        }
        Effect::HistoryLoadResult(..) => {}
        Effect::EmitDiagnostics(mut snapshot) => {
            #[cfg(feature = "bench-instrumentation")]
            let phase_started = std::time::Instant::now();
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
            #[cfg(feature = "bench-instrumentation")]
            crate::ingest_attribution::diagnostics_effect(phase_started.elapsed());
        }
        Effect::EmitReceipt(id, status) => {
            if matches!(status, WriteStatus::Signed(_) | WriteStatus::Cancelled) {
                registry.cancel_pending_write(id);
            }
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
/// `sub_id`/`filter`. It uses the same stable literal rendering helper as
/// REQ (`core::wire_sub_id_string`), while NIP-77 explicitly defines a
/// protocol namespace separate from REQ. The reducer supplies role-derived
/// ids so both can stay open concurrently, and `core::mod` resolves either
/// protocol by the exact rendered string it recorded at send time.
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
/// current generation's AUTH reached Ready via `finish_auth_ok` on its
/// exact AUTH OK), but NO reconnect preamble
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
/// `on_relay_connected`) or — for a protected session — the AUTH reducer's
/// ready transition (`finish_auth_ok`) -- an authoritative snapshot, not a
/// delta, so the
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
/// - `subscribe(LiveQuery) -> (QueryHandle, RowsReceiver)`
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
    instance: core::AuthCapabilityInstance,
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
        self.public_key == other.public_key
            && self.instance == other.instance
            && Arc::ptr_eq(&self.identity, &other.identity)
    }
}

impl Eq for SignerRegistration {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddSignerError {
    MissingPublicKey,
    CapabilityInstanceExhausted,
    RegistryFull { limit: usize },
    EngineShuttingDown,
}

impl std::fmt::Display for AddSignerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingPublicKey => f.write_str("signing capability has no public key"),
            Self::CapabilityInstanceExhausted => {
                f.write_str("AUTH capability instance space exhausted")
            }
            Self::RegistryFull { limit } => {
                write!(f, "AUTH capability registry is full at {limit} entries")
            }
            Self::EngineShuttingDown => f.write_str("engine is shutting down"),
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
    ) -> Result<(QueryHandle, RowsReceiver), EngineThreadError> {
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
    /// processes it, the row channel's sender is dropped and the app's
    /// [`RowsReceiver`] observes a clean disconnect.
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

    /// Install the authorization policy for one exact account identity.
    /// Replacing a policy returns a new opaque registration and invalidates
    /// any operation bound to the prior capability instance.
    pub fn add_auth_policy<P>(
        &self,
        expected_pubkey: PublicKey,
        policy: P,
    ) -> Result<AuthPolicyRegistration, AddAuthPolicyError>
    where
        P: AuthPolicy + 'static,
    {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::AddAuthPolicy {
                expected_pubkey,
                policy: Box::new(policy),
                reply: reply_tx,
            })
            .expect("nmp-engine: add_auth_policy() called after shutdown");
        reply_rx
            .recv()
            .expect("nmp-engine: engine thread dropped the add_auth_policy reply")
    }

    /// Remove only the policy installation proven by this registration.
    /// A stale registration cannot remove a replacement.
    pub fn remove_auth_policy(&self, registration: AuthPolicyRegistration) -> bool {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::RemoveAuthPolicy {
                registration,
                reply: reply_tx,
            })
            .expect("nmp-engine: remove_auth_policy() called after shutdown");
        reply_rx
            .recv()
            .expect("nmp-engine: engine thread dropped the remove_auth_policy reply")
    }

    /// Ask the currently active registered signer to sign one exact event,
    /// without accepting a write or touching the canonical store/outbox. A
    /// pending remote operation is cancellable through the returned handle and
    /// engine shutdown; #704 removed the admission slot — nothing is refused.
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
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::SignEvent {
                unsigned,
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
    pub fn publish(&self, intent: WriteIntent) -> Result<FifoReceiver<WriteStatus>, PublishError> {
        self.publish_tracked(intent).map(|receipt| receipt.statuses)
    }

    /// Enqueue a write and expose its stable receipt id. This synchronous
    /// round trip waits only for the local crash-atomic acceptance door,
    /// never for signing, routing, network I/O, or ACKs. Correlation-id
    /// exhaustion is returned before any stream or identity is fabricated.
    pub fn publish_tracked(&self, intent: WriteIntent) -> Result<ReceiptStream, PublishError> {
        let (tx, rx) = fifo_channel();
        let registration = ReceiptSinkRegistration::new();
        let sink: Box<dyn ReceiptSink> = Box::new(ChannelReceiptSink(tx));
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::PublishTracked {
                intent,
                sink,
                registration: registration.clone(),
                reply: reply_tx,
            })
            .expect("nmp-engine: publish called after shutdown");
        let id = reply_rx
            .recv()
            .expect("nmp-engine: engine dropped publish receipt reply")?;
        arm_receipt_sink_close(&rx, self.inbox.clone(), id, registration);
        Ok(ReceiptStream { id, statuses: rx })
    }

    /// Attach an additional observer to a retained receipt. The returned
    /// channel is primed with durable receipt/attempt facts. Missing and
    /// retained-but-unreadable evidence are distinct outcomes.
    pub fn reattach_receipt(&self, id: ReceiptId) -> ReceiptReattachment {
        self.reattach_receipt_page(id, None)
    }

    /// Continue durable replay from an identity-stable prior-page cursor.
    /// This is delivery mechanism for receipt streams, not a second write
    /// noun.
    pub fn reattach_receipt_from(
        &self,
        id: ReceiptId,
        cursor: ReceiptReplayCursor,
    ) -> ReceiptReattachment {
        self.reattach_receipt_page(id, Some(cursor))
    }

    fn reattach_receipt_page(
        &self,
        id: ReceiptId,
        cursor: Option<ReceiptReplayCursor>,
    ) -> ReceiptReattachment {
        let (tx, rx) = fifo_channel();
        let registration = ReceiptSinkRegistration::new();
        arm_receipt_sink_close(&rx, self.inbox.clone(), id, registration.clone());
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::ReattachReceipt {
                id,
                cursor,
                sink: Box::new(ChannelReceiptSink(tx)),
                registration,
                reply: reply_tx,
            })
            .expect("nmp-engine: reattach called after shutdown");
        match reply_rx
            .recv()
            .expect("nmp-engine: engine dropped reattach reply")
        {
            (ReattachOutcome::Attached, next_cursor) => ReceiptReattachment::Attached {
                id,
                statuses: rx,
                next_cursor,
            },
            (ReattachOutcome::NotFound, _) => ReceiptReattachment::NotFound,
            (ReattachOutcome::RetainedButUnreadable, _) => {
                ReceiptReattachment::RetainedButUnreadable
            }
        }
    }

    /// #591: recover a receipt after a crash that happened BEFORE the app
    /// could durably record the `ReceiptId` `publish_tracked` returned --
    /// looked up by the caller's own correlation token instead. Otherwise
    /// identical to [`Self::reattach_receipt`] (same replay/attach
    /// behavior, same `ReceiptReattachment` outcome vocabulary) -- the
    /// resolved id the caller could not otherwise learn rides along on
    /// `Attached`.
    pub fn reattach_by_correlation(&self, token: String) -> ReceiptReattachment {
        let (tx, rx) = fifo_channel();
        let registration = ReceiptSinkRegistration::new();
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::ReattachByCorrelation {
                token,
                sink: Box::new(ChannelReceiptSink(tx)),
                registration: registration.clone(),
                reply: reply_tx,
            })
            .expect("nmp-engine: reattach called after shutdown");
        match reply_rx
            .recv()
            .expect("nmp-engine: engine dropped reattach reply")
        {
            (ReattachOutcome::Attached, Some(id), next_cursor) => {
                arm_receipt_sink_close(&rx, self.inbox.clone(), id, registration);
                ReceiptReattachment::Attached {
                    id,
                    statuses: rx,
                    next_cursor,
                }
            }
            (ReattachOutcome::Attached, None, _) => {
                unreachable!(
                    "EngineCore::reattach_by_correlation always resolves an id when Attached"
                )
            }
            (ReattachOutcome::NotFound, _, _) => ReceiptReattachment::NotFound,
            (ReattachOutcome::RetainedButUnreadable, _, _) => {
                ReceiptReattachment::RetainedButUnreadable
            }
        }
    }

    /// Explicitly cancel one accepted unsigned write. A successful outcome
    /// means the durable `Cancelled` fact observers receive and reattachment
    /// replays committed.
    pub fn cancel_write(&self, id: ReceiptId) -> Result<CancelWriteOutcome, CancelWriteError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::CancelWrite {
                id,
                reply: reply_tx,
            })
            .map_err(|_| CancelWriteError::EngineClosed)?;
        reply_rx
            .recv()
            .map_err(|_| CancelWriteError::EngineClosed)?
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

    #[cfg(test)]
    fn receipt_sink_count(&self, id: ReceiptId) -> usize {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::ReceiptSinkCount {
                id,
                reply: reply_tx,
            })
            .expect("nmp-engine: receipt sink census called after shutdown");
        reply_rx
            .recv()
            .expect("nmp-engine: engine dropped receipt sink census reply")
    }

    /// Stop the engine thread (and, transitively, its bridge threads — see
    /// [`EngineThread::join`]). Idempotent: a `Handle` clone calling this
    /// after another already has just finds the inbox gone and no-ops.
    pub fn shutdown(&self) {
        let _ = self.inbox.send(Cmd::Shutdown);
    }
}
