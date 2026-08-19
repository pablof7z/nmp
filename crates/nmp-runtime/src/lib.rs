//! The async edge (plan §2 position 2). `EngineThread` owns two dedicated OS
//! threads plus one fixed two-worker async runtime:
//!
//! - the **engine thread**, which owns `core::EngineCore` and runs a
//!   deadline-armed blocking recv loop (D8: the existing blocking
//!   `mpsc::Receiver<Cmd>::recv()` grows a timeout, never a poll-loop timer
//!   thread — see `engine_loop`'s doc and
//!   `docs/design/retraction-and-negative-deltas.md` §3.3, #39): with no
//!   deadline pending it blocks on plain `recv()`; with one pending it
//!   `recv_timeout`s exactly until the earliest reducer, NIP-11, pending
//!   wire-admission, or diagnostics-delivery deadline. A timeout dispatches
//!   only the due owner before re-arming from the freshly-recomputed minimum
//!   — for every command it calls `EngineCore::handle`/`::tick` and dispatches
//!   the returned `core::Effect`s to `nmp_transport::Pool::send`, the
//!   `nmp_signer` capability, and the app-facing channels;
//! - the **pool-bridge thread**, a tiny translator that blocking-`recv`s
//!   `nmp_transport::PoolEvent`s (the pool's OWN `mio` worker threads push
//!   these) and forwards each as a `core::EngineMsg` onto the engine
//!   thread's inbox;
//! - the **adapter runtime**, whose fixed workers host waker-driven NIP-11,
//!   signer, AUTH, and platform-adapter tasks. Logical concurrency changes
//!   task count, not runtime-thread count; private subsystem bounds control
//!   physical network/body/queue work.
//!
//! `Handle` is the cheap, `Clone + Send` value the app holds: it sends
//! command `EngineMsg`s in (wrapped in the runtime-private [`Cmd`] envelope)
//! and gets back plain channels. The threading is entirely interior — the
//! app never sees `mio`, never sees a `PoolEvent`, never adopts a runtime
//! (§2, P1). `EngineCore` itself is `!Send`-friendly (M1's resolver keeps an
//! `Rc<RefCell<>>`) — it is constructed INSIDE the engine thread's closure
//! and never crosses a thread boundary; only `Send + 'static` VALUES (the
//! store, the neutral routing facts, the signer) are moved into that closure at spawn
//! time.
//!
//! ## One reducer-to-runtime delivery path
//!
//! Core emits each row, window, and receipt fact exactly once as a typed
//! `Effect`. Runtime owns every live mailbox registry and is the only layer
//! that sends those facts to app-facing channels. Receipt registrations also
//! retain the identity-stable durable replay cursor after the last fact their
//! finite FIFO actually accepted, so a lag recovery can resume without
//! replaying facts already delivered to that consumer.
//!
//! ## One public-read replay owner
//!
//! `EngineCore` is the sole owner of live public REQ state and replays the
//! current plan exactly once from `RelayConnected`. This runtime therefore
//! keeps the transport reconnect preamble empty for NMP read sessions. A
//! second automatic owner here would race the reducer's generation-aware
//! replay and make an unchanged `(session, sub-id, filter)` reach the relay
//! two or three times. An independently owned signer-provider transport may
//! retain its own reconnect preamble; this rule does not alter that separate
//! capability contract.

mod auth;
mod clock;
mod diagnostics_channel;
mod diagnostics_delivery;
mod engine_thread;
mod fifo_channel;
mod history_mailbox;
// The engine thread's owner for identity-session membership and signing
// capability, moved beside the loop that drives it (#1731) — same treatment
// as `nip11_decision` and `wire_admission` below.
mod identity_sessions;
// The opaque app-owned session payload and its signer descriptors. It came
// with the runtime rather than staying in `nmp` because `EngineThread::spawn`
// takes a `RestoredSession` and `Handle` owns the live session state; the
// facade only encodes, decodes, and hands one over.
pub mod session;
// The NIP-11 snapshot -> reducer-evidence projection: the glue belongs
// beside the loop, so the only edge runs downward into the protocol crate.
// It is now this crate's ONE declared protocol edge. The NIP-65 half used to
// sit beside it as a second, cargo-feature-gated one; author-route discovery
// is an application-supplied `AuthorRouteProvider` now, so this crate names
// no routing protocol at all.
mod nip11;
// The NIP-11 grace-fallback deadline (#1731) — a different concern from
// `nip11` above: this is the state machine, not the value projection.
mod nip11_decision;
mod pool_bridge;
mod receipt_stream;
mod request_wire;
mod row_channel;
mod sign_event;
// The exponential store-recovery backoff schedule (#1731).
// The 10ms wire-admission window (#1731).
mod wire_admission;

pub use clock::EngineClock;
pub use engine_thread::{
    EngineThread, EngineThreadError, RuntimeConfig, DEFAULT_MAX_AUTH_CAPABILITIES,
};
pub use history_mailbox::{AsyncHistoryReceiver, HistoryMsg, HistoryReceiver};
pub use identity_sessions::RuntimeSessionExportSources;
pub use receipt_stream::{ReceiptReattachment, ReceiptStream};
pub use sign_event::{SignEventCancel, SignEventError, SignEventOperation};

pub use auth::{
    AddAuthPolicyError, AuthPolicy, AuthPolicyDecision, AuthPolicyError, AuthPolicyOp,
    AuthPolicyPendingSender, AuthPolicyRegistration, AuthPolicyRequest, AuthPolicyResolveError,
    AuthPolicyResult, PendingAuthPolicyOp,
};

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
// #1624 removed this module's only production thread spawn; the inline
// `pool_bridge_tests` module is what still spawns.
use std::time::{Duration, Instant};

use crossbeam_channel as cb;
use nmp_grammar::LiveQuery;
use nmp_router::WireOp;
use nmp_signer::{SignerOp, SigningCapability};
use nmp_store::RedbStore;
use nostr::{
    ClientMessage, EventId, JsonUtil, PublicKey, RelayUrl, Timestamp, UnsignedEvent,
};

use nmp_transport::{
    DurableSendOutcome, HandoffResult, Pool, RelayFrame, RelaySessionKey, WireFrame,
};

use crate::session::{
    RestoredSession, SessionAccount, SessionProvider, SessionSnapshot, SigningAvailability,
};
#[doc(hidden)]
pub use nmp_engine::core::ReceiptReplayCursor;
use nmp_engine::core::{
    self, AcquisitionEvidence, AuthorRouteProvider, DiagnosticsSnapshot, Effect, EngineCore,
    EngineMsg, HistoryAdvanceError, HistoryQuery, HistorySessionId,
    ObservationEvidence, ObservationId, ObservationOpen, ProviderReroot, PublishError,
    PublishPreparation, ReattachOutcome, ReceiptId, RowDelta,
};
use nmp_engine::publish_queue::{
    CancelWriteError, CancelWriteOutcome, PublishQueueEntry, PublishQueueReadError,
    RemoveQueueEntryError, SigningState, WriteFact, WriteOutcome,
};
use nmp_grammar::WriteIntent;
use nmp_nip11::{
    RelayInformationCachePolicy, RelayInformationError, RelayInformationService,
    RelayInformationSnapshot,
};

use diagnostics_channel::{latest_channel, LatestSender};
pub use diagnostics_channel::{AsyncLatestReceiver, ConcurrentNext, LatestReceiver};
use diagnostics_delivery::{
    fan_out as fan_out_diagnostics, flush_due as flush_due_diagnostics,
    seed_observer as seed_diagnostics_observer,
    snapshot_with_pool as diagnostics_snapshot_with_pool, DiagnosticsDeliveryState,
};
pub use fifo_channel::{
    fifo_channel, superseding_fifo_channel, AsyncFifoReceiver, FifoNextError, FifoReceiver,
    FifoRecvError, FifoRecvTimeoutError, FifoSender, FifoTryRecvError, FACT_CHANNEL_CAPACITY,
};
use identity_sessions::{decode_signed_event, RuntimeSessionState, SignerRegistry};
use nip11_decision::Nip11DecisionState;
use pool_bridge::{pool_bridge_loop, translate_pool_event, EnginePoolSink};
use receipt_stream::{
    deliver_receipt_replay_page, publish_result, take_publish_replay, ReceiptDeliveryRegistration,
    ReceiptDeliveryRegistry,
};
use request_wire::{apply_replay, apply_wire_delta, close_frame_text};
use row_channel::{rows_channel, RowsSender};
pub use row_channel::{AsyncRowsReceiver, RowsReceiver};
use wire_admission::WireAdmissionState;

struct EnginePoolRuntime {
    pool: Pool,
    stop: cb::Sender<()>,
    /// #704: the engine-owned multi-thread tokio runtime that hosts every
    /// adapter task (signer/AUTH completion awaits, NIP-11 fetches, optional
    /// provider sessions, follow-action). Replaces the deleted blocking-adapter
    /// executor.
    runtime: Arc<tokio::runtime::Runtime>,
    relay_information: RelayInformationService,
    max_auth_capabilities: usize,
    max_publish_attempts: u64,
    /// The application's chosen author-route algorithm, or `None` for an
    /// engine that discovers no routes at all (operator lanes and explicit
    /// routes still carry everything they carry).
    route_provider: Option<Box<dyn AuthorRouteProvider>>,
}

/// One delivered batch for a live subscription: an exact row transition
/// rebased onto the receiver's previous batch + the query's latest per-source
/// acquisition evidence (see [`RowsReceiver`] and the module doc's "One
/// reducer-to-runtime delivery path" note).
pub type RowsMsg = (
    Vec<RowDelta>,
    Vec<AcquisitionEvidence>,
    Vec<ObservationEvidence>,
);

// A runtime-level integration falsifier: it spawns a real `EngineThread` and
// asserts typed refusals reach the app. It lived under `core/` and reached
// back up through `crate::*` to do it, which was the only
// `core -> runtime` edge in the crate (#1142 boundary cleanup).

/// The app-facing handle to a live subscription (returned by
/// [`Handle::subscribe`]). `Send`, `Copy`-cheap, carries nothing that
/// borrows into the engine thread — it is exactly the correlation id
/// [`Handle::unsubscribe`] needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryHandle(ObservationId);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryHandle(HistorySessionId);

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
        reply: Sender<Result<(ObservationId, RowsReceiver), EngineThreadError>>,
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
        sender: FifoSender<WriteFact>,
        registration: ReceiptDeliveryRegistration,
        /// The whole acceptance answer: the receipt id the store issued and
        /// the event id it froze, decided together and reported together.
        reply: Sender<Result<(ReceiptId, EventId), PublishError>>,
    },

    ReattachReceipt {
        id: ReceiptId,
        cursor: Option<ReceiptReplayCursor>,
        sender: FifoSender<WriteFact>,
        registration: ReceiptDeliveryRegistration,
        reply: Sender<(ReattachOutcome, Option<ReceiptReplayCursor>)>,
    },
    DetachReceiptDelivery {
        id: ReceiptId,
        registration: ReceiptDeliveryRegistration,
    },
    #[cfg(feature = "bench-instrumentation")]
    ObservationOwnershipCensus {
        reply: Sender<ObservationOwnershipCensus>,
    },
    /// Hold the reducer inside one command turn so a test can observe whether
    /// a simultaneously-due core deadline ran before command dispatch.
    #[cfg(feature = "bench-instrumentation")]
    DeadlineRaceProbe {
        at: Timestamp,
        entered: Sender<()>,
        release: Receiver<()>,
    },
    CancelWrite {
        id: ReceiptId,
        reply: Sender<Result<CancelWriteOutcome, CancelWriteError>>,
    },
    /// #1039: read the app's own publish queue back.
    PublishQueueEntries {
        event_id: Option<EventId>,
        after: Option<ReceiptId>,
        limit: u8,
        reply: Sender<Result<Vec<PublishQueueEntry>, PublishQueueReadError>>,
    },
    /// #1039: forget one queue entry. A termination path, not housekeeping.
    RemovePublishQueueEntry {
        id: ReceiptId,
        reply: Sender<Result<(), RemoveQueueEntryError>>,
    },
    /// Register a new signing capability (M4 §5: `SignerRegistry`). The
    /// reply carries the pubkey the engine thread's registry keyed it under,
    /// or a typed error if the capability has no stable identity.
    AddSigner {
        signer: Box<dyn SigningCapability + Send + Sync>,
        reply: Sender<Result<SignerRegistration, AddSignerError>>,
    },
    RemoveSigner {
        registration: SignerRegistration,
        reply: Sender<bool>,
    },
    SessionSnapshot {
        reply: Sender<SessionSnapshot>,
    },
    SessionExportSources {
        reply: Sender<RuntimeSessionExportSources>,
    },
    CurrentSessionPubkey {
        reply: Sender<Option<PublicKey>>,
    },
    AddPrivateKeyAccount {
        signer: nmp_local_signer::LocalKeySigner,
        make_current: bool,
        reply: Sender<Result<SessionAccount, AddSignerError>>,
    },
    AddPublicKeyAccount {
        public_key: PublicKey,
        make_current: bool,
        reply: Sender<SessionAccount>,
    },
    MakeCurrentAccount {
        public_key: PublicKey,
        reply: Sender<bool>,
    },
    RemoveSessionAccount {
        public_key: PublicKey,
        reply: Sender<bool>,
    },
    ClearSession {
        reply: Sender<()>,
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
    /// Sign one exact event through the current account's registered
    /// capability without entering the write/store/delivery reducer.
    SignEvent {
        unsigned: UnsignedEvent,
        completion: sign_event::SignEventCompletion,
        reply: Sender<Result<sign_event::SignEventRegistration, SignEventError>>,
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

#[cfg(feature = "bench-instrumentation")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[doc(hidden)]
pub struct ObservationOwnershipCensus {
    handles: usize,
    histories: usize,
    history_handles: usize,
    resolver_nodes: usize,
    demand_atoms: usize,
    planned_sessions: usize,
    pending_execution_owners: usize,
    active_execution_owners: usize,
    live_wire_owners: usize,
    row_channels: usize,
    history_channels: usize,
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

// Moved here with the wiring they exercise: these drive a real `EngineCore`
// through a provider and the loop's own reroot/apply path, which is not
// reachable from anywhere else.

#[derive(Clone, Copy)]
struct DispatchRuntime<'a> {
    self_inbox: &'a Sender<Cmd>,
    relay_information: &'a RelayInformationService,
    runtime: &'a tokio::runtime::Handle,
    nip11_decisions: &'a RefCell<Nip11DecisionState>,
    wire_admission: &'a RefCell<WireAdmissionState>,
    diagnostics_delivery: &'a RefCell<DiagnosticsDeliveryState>,
    auth_policies: &'a RefCell<auth::AuthPolicyRegistry>,
    auth_tasks: &'a RefCell<auth::AuthTaskRegistry>,
    receipt_deliveries: &'a RefCell<ReceiptDeliveryRegistry>,
    route_provider: &'a RefCell<Option<RouteProviderSlot>>,
}

/// The provider the application constructed, plus the observation the LOOP
/// opened on its behalf.
///
/// The handle lives HERE rather than inside the provider: it is loop
/// mechanics, not provider policy. A provider therefore cannot mint an
/// observation id, cannot keep a stale one, and cannot claim a delivery that
/// is not its own — by construction rather than by review.
struct RouteProviderSlot {
    provider: Box<dyn AuthorRouteProvider>,
    bound: Option<ObservationId>,
    /// Set once the provider panics inside any of the three synchronous
    /// calls the loop makes into it (see `guarded_provider_call`). Unwind
    /// safety says nothing about the provider's state after a caught panic,
    /// so a poisoned provider is never called again for the life of the
    /// engine -- refused the same way a provider that answers with silence
    /// already is. See #1802.
    poisoned: bool,
}

impl RouteProviderSlot {
    fn new(provider: Box<dyn AuthorRouteProvider>) -> Self {
        Self {
            provider,
            bound: None,
            poisoned: false,
        }
    }
}

/// Runs one synchronous call into the foreign `AuthorRouteProvider`,
/// catching a panic instead of letting it unwind through the reducer
/// thread. `AuthorRouteProvider` is app-supplied code invoked under a
/// `RefCell` borrow with no timeout and no capability token (#1802) --
/// a caught panic is the one failure mode a synchronous foreign call can
/// hit that isn't already a typed return value, so this converts it into
/// one: the provider is poisoned and every caller sees `None`, which is
/// indistinguishable from "no provider" to the rest of the loop.
fn guarded_provider_call<T>(
    slot: &mut RouteProviderSlot,
    call: impl FnOnce(&mut dyn AuthorRouteProvider) -> T,
) -> Option<T> {
    if slot.poisoned {
        return None;
    }
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        call(slot.provider.as_mut())
    })) {
        Ok(value) => Some(value),
        Err(_) => {
            slot.poisoned = true;
            None
        }
    }
}

/// The three wires the engine thread owns, and the one thing they have in
/// common: each is a way for something OUTSIDE the reducer to reach it. The
/// commands an app sends (`cmd_rx`), the ones the runtime posts to itself
/// (`self_inbox`), and the wall clock its `Tick`s carry -- which is wired to
/// that same inbox, since stating the time also delivers it (see
/// [`EngineClock`]).
///
/// One struct rather than three parameters because they arrive together, are
/// destructured immediately, and travel nowhere else.
struct EngineWiring<'a> {
    clock: &'a EngineClock,
    cmd_rx: &'a Receiver<Cmd>,
    self_inbox: &'a Sender<Cmd>,
    startup_ready: Sender<()>,
    /// Published once per pass of [`engine_loop`]'s wait — see
    /// [`EngineThread::wait_arms`] for what reads it and why.
    wait_arms: Arc<AtomicU64>,
}

/// Shutdown is finished when no lifecycle can still run FOREIGN code.
///
/// Two owners can, and they are the only two: an AUTH policy or signer task
/// awaiting an app-provided capability, and a sign-only operation whose
/// completion closure has not been delivered. Each answers for its own state;
/// this composes their answers.
///
/// It exists as one named function because the composition is asked twice —
/// once when `Cmd::Shutdown` arrives and once per drained command afterwards —
/// and two hand-written copies of the same conjunction are two places for a
/// third owner to be forgotten. "Drained" is deliberately not "empty": an
/// operation exempted by a reentrant `join()` is gone from the drain while its
/// completion is still running, which is correct and is the whole reason this
/// predicate is not `is_empty()` on one map.
fn foreign_work_drained(
    auth_tasks: &RefCell<auth::AuthTaskRegistry>,
    sign_events: &sign_event::ActiveSignEvents,
) -> bool {
    auth_tasks.borrow().is_drained() && sign_events.is_drained()
}

/// The engine thread's body: construct `EngineCore` (this is the ONLY place
/// it is ever built — it never leaves this stack frame), then block on
/// `cmd_rx` (D8) until `Cmd::Shutdown`.
///
/// The deadline-armed driver (§3.3, #39): every iteration re-reads the core
/// plus the NIP-11, wire-admission, and deferred-diagnostics deadlines, then
/// waits for their exact minimum. A command that introduces an earlier
/// deadline re-arms naturally on the next iteration; there is no polling or
/// sleeper. `None` blocks on plain `recv()`. A timeout fires only the due
/// owners, then recomputes the minimum.
fn engine_loop(
    store: RedbStore,
    routing_facts: nmp_engine::core::RoutingFactStore,
    cap: usize,
    initial_session: RestoredSession,
    capabilities: Vec<nmp_grammar::ReplaceableMaterializerSpec>,
    pool_runtime: EnginePoolRuntime,
    wiring: EngineWiring<'_>,
) {
    let EngineWiring {
        clock,
        cmd_rx,
        self_inbox,
        startup_ready,
        wait_arms,
    } = wiring;
    let EnginePoolRuntime {
        pool,
        stop: pool_stop_tx,
        runtime,
        relay_information,
        max_auth_capabilities,
        max_publish_attempts,
        route_provider,
    } = pool_runtime;
    let runtime_handle = runtime.handle().clone();
    let runtime_handle = &runtime_handle;
    let mut core = EngineCore::new_with_routing_facts(store, routing_facts, cap)
        .with_max_publish_attempts(max_publish_attempts);
    core.install_replaceable_materializers(capabilities);
    let mut row_channels: HashMap<ObservationId, RowsSender> = HashMap::new();
    let mut history_channels: HashMap<HistorySessionId, LatestSender<HistoryMsg>> = HashMap::new();
    let mut diag_channels: HashMap<u64, LatestSender<DiagnosticsSnapshot>> = HashMap::new();
    let mut next_diag_id: u64 = 0;
    let mut auth_instances = auth::AuthCapabilityInstances::default();
    let mut registry = RuntimeSessionState::default();
    for account in initial_session.accounts {
        match account.signer {
            Some(signer) => {
                let instance = auth_instances
                    .mint()
                    .expect("preflighted initial session fits fresh capability instance space");
                registry.note_account(account.public_key, Some(SessionProvider::LocalKey));
                registry.add_local(account.public_key, instance, signer);
            }
            None => {
                registry.note_account(account.public_key, None);
            }
        }
    }
    let auth_policies = RefCell::new(auth::AuthPolicyRegistry::default());
    let auth_tasks = RefCell::new(auth::AuthTaskRegistry::default());
    let receipt_deliveries = RefCell::new(ReceiptDeliveryRegistry::default());
    let route_provider = RefCell::new(route_provider.map(RouteProviderSlot::new));
    let mut active_sign_events = sign_event::ActiveSignEvents::default();
    let nip11_decisions = RefCell::new(Nip11DecisionState::default());
    let wire_admission = RefCell::new(WireAdmissionState::default());
    let diagnostics_delivery = RefCell::new(DiagnosticsDeliveryState::default());
    let dispatch_runtime = DispatchRuntime {
        self_inbox,
        relay_information: &relay_information,
        runtime: runtime_handle,
        nip11_decisions: &nip11_decisions,
        wire_admission: &wire_admission,
        diagnostics_delivery: &diagnostics_delivery,
        auth_policies: &auth_policies,
        auth_tasks: &auth_tasks,
        receipt_deliveries: &receipt_deliveries,
        route_provider: &route_provider,
    };

    // The fully decoded session is installed before recovery. In particular,
    // a recovered parked write observes its provider and current selection on
    // the very first recovery effect; there is no externally visible empty-
    // session turn and no post-start restore command.
    //
    // The selection comes straight from the decoded payload (#1657). It used to
    // be stored on `registry` first and read back here, which made the runtime
    // authoritative at boot and the reducer authoritative afterwards, with
    // nothing expressing the handover.
    let initial_selection_effects =
        core.handle(EngineMsg::SetActivePubkey(initial_session.current_pubkey));
    dispatch_core_effects(
        &mut core,
        initial_selection_effects,
        &pool,
        &mut row_channels,
        &mut history_channels,
        &mut diag_channels,
        &registry,
        dispatch_runtime,
    );
    // Recovery happens before the first externally-issued command. Give that
    // transition current wall truth before it rebuilds deadline-bearing lane
    // state; this is a cheap assignment, while recovery itself remains the
    // explicit owner of its durable startup work.
    core.advance_clock(clock.now());
    let recovery_effects = core.recover_on_boot();
    dispatch_core_effects(
        &mut core,
        recovery_effects,
        &pool,
        &mut row_channels,
        &mut history_channels,
        &mut diag_channels,
        &registry,
        dispatch_runtime,
    );
    // Recovery reconstructs parked obligations before it can observe a live
    // provider transition. Replay that transition while startup is still
    // closed so every recovered obligation is re-armed before the constructor
    // returns.
    for public_key in registry.provider_pubkeys() {
        let effects = core.handle(EngineMsg::SignerAttached(public_key));
        dispatch_core_effects(
            &mut core,
            effects,
            &pool,
            &mut row_channels,
            &mut history_channels,
            &mut diag_channels,
            &registry,
            dispatch_runtime,
        );
    }
    let _ = startup_ready.send(());

    let mut shutting_down = false;
    loop {
        // One increment per pass, before the wait this pass arms. A loop
        // parked in `recv()`/`recv_timeout(wait)` has not reached here again,
        // so the count standing still IS "still blocked on the same wait" --
        // which is what `EngineThread::wait_arms` exists to let a test read.
        wait_arms.fetch_add(1, Ordering::Relaxed);
        // A continuously-ready command stream must not starve a delivery
        // deadline. This command-boundary check is still event-driven; the
        // timeout arm below owns the idle-engine case.
        flush_due_diagnostics(
            &core,
            &pool,
            &diag_channels,
            &diagnostics_delivery,
            Instant::now(),
        );
        let core_deadline = core.next_deadline();
        let core_wait = core_deadline.map(|deadline| duration_until(deadline, clock.now()));
        let nip11_wait = nip11_decisions
            .borrow()
            .next_deadline()
            .map(|deadline| deadline.saturating_duration_since(Instant::now()));
        let wire_admission_wait = wire_admission
            .borrow()
            .next_deadline()
            .map(|deadline| deadline.saturating_duration_since(Instant::now()));
        let diagnostics_wait = diagnostics_delivery
            .borrow()
            .next_deadline()
            .map(|deadline| deadline.saturating_duration_since(Instant::now()));
        let wait = if shutting_down {
            None
        } else {
            [
                core_wait,
                nip11_wait,
                wire_admission_wait,
                diagnostics_wait,
            ]
            .into_iter()
            .flatten()
            .min()
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
                    let wall_now = clock.now();
                    core.advance_clock(wall_now);
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
                            &registry,
                            dispatch_runtime,
                        );
                    }
                    if wire_admission.borrow_mut().take_due(Instant::now()) {
                        let effects = core.handle(EngineMsg::FlushWireAdmission(wall_now));
                        dispatch_core_effects(
                            &mut core,
                            effects,
                            &pool,
                            &mut row_channels,
                            &mut history_channels,
                            &mut diag_channels,
                            &registry,
                            dispatch_runtime,
                        );
                    }
                    let due = core
                        .next_deadline()
                        .is_some_and(|deadline| deadline <= wall_now);
                    if due {
                        let effects = core.handle(EngineMsg::Tick(wall_now));
                        dispatch_core_effects(
                            &mut core,
                            effects,
                            &pool,
                            &mut row_channels,
                            &mut history_channels,
                            &mut diag_channels,
                            &registry,
                            dispatch_runtime,
                        );
                    }
                    flush_due_diagnostics(
                        &core,
                        &pool,
                        &diag_channels,
                        &diagnostics_delivery,
                        Instant::now(),
                    );
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            },
        };
        #[cfg(feature = "bench-instrumentation")]
        if let Cmd::DeadlineRaceProbe { at, .. } = &cmd {
            // Model the exact boundary where the command and an armed core
            // deadline become ready together. The ordinary clock setter
            // queues its own Tick and would pre-order this test.
            clock.pin_silently(*at);
        }
        let command_wall_now = clock.now();
        let command_is_tick = matches!(&cmd, Cmd::Engine(EngineMsg::Tick(_)));
        if !shutting_down
            && !command_is_tick
            && core_deadline.is_some_and(|deadline| deadline <= command_wall_now)
        {
            // A queued command can win recv_timeout at the instant its core
            // deadline becomes ready. Consume the deadline once first.
            let effects = core.handle(EngineMsg::Tick(command_wall_now));
            dispatch_core_effects(
                &mut core,
                effects,
                &pool,
                &mut row_channels,
                &mut history_channels,
                &mut diag_channels,
                &registry,
                dispatch_runtime,
            );
        }
        if !command_is_tick {
            // Every non-Tick reducer transition sees current wall truth. This
            // is O(1) and never executes maintenance.
            core.advance_clock(command_wall_now);
        }
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

                Cmd::AddPrivateKeyAccount { reply, .. } => {
                    let _ = reply.send(Err(AddSignerError::EngineShuttingDown));
                }
                Cmd::SessionSnapshot { reply } => {
                    let _ = reply.send(registry.snapshot(core.active_pubkey()));
                }
                Cmd::SessionExportSources { reply } => {
                    let _ = reply.send(registry.export_sources(core.active_pubkey()));
                }
                Cmd::CurrentSessionPubkey { reply } => {
                    let _ = reply.send(core.active_pubkey());
                }
                Cmd::AddPublicKeyAccount {
                    public_key, reply, ..
                } => {
                    let account = SessionAccount {
                        public_key,
                        provider: None,
                        signing: SigningAvailability::Unsupported,
                    };
                    let _ = reply.send(account);
                }
                Cmd::MakeCurrentAccount { reply, .. } => {
                    let _ = reply.send(false);
                }
                Cmd::RemoveSessionAccount { reply, .. } => {
                    let _ = reply.send(false);
                }
                Cmd::ClearSession { reply } => {
                    let _ = reply.send(());
                }
                Cmd::Subscribe { reply, .. } => {
                    let _ = reply.send(Err(EngineThreadError::EngineShuttingDown));
                }
                Cmd::SubscribeHistory { reply, .. } => {
                    let _ = reply.send(Err(EngineThreadError::EngineShuttingDown));
                }
                // Dropping this reply makes `Handle::request_rows` return
                // `None`, which the facade truthfully maps to `EngineClosed`.
                Cmd::RequestRows { .. } => {}
                Cmd::PublishTracked { reply, .. } => {
                    let _ = reply.send(Err(PublishError::EngineShuttingDown));
                }
                Cmd::PublishQueueEntries { reply, .. } => {
                    let _ = reply.send(Err(PublishQueueReadError::EngineClosed));
                }
                Cmd::RemovePublishQueueEntry { reply, .. } => {
                    let _ = reply.send(Err(RemoveQueueEntryError::EngineClosed));
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
                    sender,
                    registration,
                    reply,
                } => {
                    let page = core.reattach_receipt_page(id, cursor, FACT_CHANNEL_CAPACITY);
                    let found = deliver_receipt_replay_page(
                        &core,
                        &mut receipt_deliveries.borrow_mut(),
                        id,
                        sender,
                        registration,
                        page,
                    );
                    let _ = reply.send(found);
                }
                Cmd::DetachReceiptDelivery { id, registration } => {
                    receipt_deliveries.borrow_mut().detach(id, &registration);
                }
                #[cfg(feature = "bench-instrumentation")]
                Cmd::ObservationOwnershipCensus { reply } => {
                    let core_census = core.observation_ownership_census();
                    let _ = reply.send(ObservationOwnershipCensus {
                        handles: core_census.handles,
                        histories: core_census.histories,
                        history_handles: core_census.history_handles,
                        resolver_nodes: core_census.resolver_nodes,
                        demand_atoms: core_census.demand_atoms,
                        planned_sessions: core_census.planned_sessions,
                        pending_execution_owners: core_census.pending_execution_owners,
                        active_execution_owners: core_census.active_execution_owners,
                        live_wire_owners: core_census.live_wire_owners,
                        row_channels: row_channels.len(),
                        history_channels: history_channels.len(),
                    });
                }
                Cmd::ObserveDiagnostics { reply } => {
                    let id = next_diag_id;
                    next_diag_id = next_diag_id.saturating_add(1);
                    let (tx, rx) = latest_channel();
                    seed_diagnostics_observer(
                        diagnostics_snapshot_with_pool(&core, &pool),
                        &tx,
                        &diag_channels,
                        &diagnostics_delivery,
                    );
                    if reply.send((id, rx)).is_ok() {
                        diag_channels.insert(id, tx);
                    }
                }
                Cmd::RelayBatch { applied, .. } => {
                    let _ = applied.send(());
                }
                Cmd::CancelSignEvent(id) | Cmd::SignEventFinished(id) => {
                    active_sign_events.cancel(id);
                }
                Cmd::ExemptSignEventDrain(op_id) => {
                    active_sign_events.exempt_from_shutdown_drain(op_id);
                }
                #[cfg(feature = "bench-instrumentation")]
                Cmd::DeadlineRaceProbe { .. } => {}
                Cmd::Engine(_)
                | Cmd::RelayInformationFetched { .. }
                | Cmd::RelayWorkerRetired
                | Cmd::UnobserveDiagnostics(_)
                | Cmd::UnsubscribeHistory(_)
                | Cmd::Shutdown => {}
            }
            if foreign_work_drained(&auth_tasks, &active_sign_events) {
                break;
            }
            continue;
        }
        match cmd {
            Cmd::Shutdown => {
                shutting_down = true;

                auth_tasks.borrow_mut().shutdown();
                registry.cancel_all_pending_writes();
                active_sign_events.cancel_for_shutdown();
                if foreign_work_drained(&auth_tasks, &active_sign_events) {
                    break;
                }
            }
            Cmd::ExemptSignEventDrain(op_id) => {
                active_sign_events.exempt_from_shutdown_drain(op_id);
            }
            Cmd::RelayInformationFetched {
                url,
                generation,
                result,
            } => {
                if !nip11_decisions.borrow_mut().complete(&url, generation) {
                    continue;
                }
                let information = (*result).ok().as_ref().map(nip11::capability_evidence);
                let effects = core.handle(EngineMsg::RelayInformationResolved(url, information));
                dispatch_core_effects(
                    &mut core,
                    effects,
                    &pool,
                    &mut row_channels,
                    &mut history_channels,
                    &mut diag_channels,
                    &registry,
                    dispatch_runtime,
                );
            }
            Cmd::RelayBatch { frames, applied } => {
                #[cfg(feature = "bench-instrumentation")]
                let batch_started = std::time::Instant::now();
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
                        &registry,
                        dispatch_runtime,
                    );
                }
                #[cfg(feature = "bench-instrumentation")]
                nmp_engine::ingest_attribution::engine_batch_process(batch_started.elapsed());
                let _ = applied.send(());
            }
            Cmd::AddSigner { signer, reply } => {
                let result = signer
                    .public_key()
                    .ok_or(AddSignerError::MissingPublicKey)
                    .map(|public_key| PublicKey::from_byte_array(*public_key.as_bytes()))
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

            Cmd::SessionSnapshot { reply } => {
                let _ = reply.send(registry.snapshot(core.active_pubkey()));
            }
            Cmd::SessionExportSources { reply } => {
                let _ = reply.send(registry.export_sources(core.active_pubkey()));
            }
            Cmd::CurrentSessionPubkey { reply } => {
                let _ = reply.send(core.active_pubkey());
            }
            Cmd::AddPrivateKeyAccount {
                signer,
                make_current,
                reply,
            } => {
                let public_key = signer
                    .public_key()
                    .and_then(|key| PublicKey::from_slice(key.as_bytes()).ok())
                    .expect("local key signer always has a validated public key");
                let live = registry.len().saturating_add(auth_policies.borrow().len());
                if !registry.contains(public_key) && live >= max_auth_capabilities {
                    let _ = reply.send(Err(AddSignerError::RegistryFull {
                        limit: max_auth_capabilities,
                    }));
                    continue;
                }
                let Some(instance) = auth_instances.mint() else {
                    let _ = reply.send(Err(AddSignerError::CapabilityInstanceExhausted));
                    continue;
                };
                let (_, replaced) = registry.add_local(public_key, instance, signer);
                registry.note_account(public_key, Some(SessionProvider::LocalKey));
                let mut effects = Vec::new();
                if let Some(old_instance) = replaced {
                    auth_tasks.borrow_mut().cancel_capability(
                        public_key,
                        core::AuthCapability::Signer,
                        old_instance,
                    );
                    effects.extend(core.handle(EngineMsg::AuthCapabilityInvalidated(
                        public_key,
                        core::AuthCapability::Signer,
                        old_instance,
                    )));
                }
                effects.extend(core.handle(EngineMsg::SignerAttached(public_key)));
                if make_current {
                    effects.extend(core.handle(EngineMsg::SetActivePubkey(Some(public_key))));
                }
                dispatch_core_effects(
                    &mut core,
                    effects,
                    &pool,
                    &mut row_channels,
                    &mut history_channels,
                    &mut diag_channels,
                    &registry,
                    dispatch_runtime,
                );
                let _ = reply.send(Ok(SessionAccount {
                    public_key,
                    provider: Some(SessionProvider::LocalKey),
                    signing: SigningAvailability::Available,
                }));
            }
            Cmd::AddPublicKeyAccount {
                public_key,
                make_current,
                reply,
            } => {
                registry.ensure_account(public_key);
                if make_current {
                    let effects = core.handle(EngineMsg::SetActivePubkey(Some(public_key)));
                    dispatch_core_effects(
                        &mut core,
                        effects,
                        &pool,
                        &mut row_channels,
                        &mut history_channels,
                        &mut diag_channels,
                        &registry,
                        dispatch_runtime,
                    );
                }
                let _ = reply.send(
                    registry
                        .snapshot(core.active_pubkey())
                        .accounts
                        .into_iter()
                        .find(|account| account.public_key == public_key)
                        .expect("inserted session account"),
                );
            }
            Cmd::MakeCurrentAccount { public_key, reply } => {
                let found = registry.contains_account(public_key);
                if found {
                    let effects = core.handle(EngineMsg::SetActivePubkey(Some(public_key)));
                    dispatch_core_effects(
                        &mut core,
                        effects,
                        &pool,
                        &mut row_channels,
                        &mut history_channels,
                        &mut diag_channels,
                        &registry,
                        dispatch_runtime,
                    );
                }
                let _ = reply.send(found);
            }
            Cmd::RemoveSessionAccount { public_key, reply } => {
                let existed = registry.remove_account(public_key);
                let removed_instance = registry.remove_key(public_key);
                let mut effects = Vec::new();
                if let Some(instance) = removed_instance {
                    auth_tasks.borrow_mut().cancel_capability(
                        public_key,
                        core::AuthCapability::Signer,
                        instance,
                    );
                    effects.extend(core.handle(EngineMsg::AuthCapabilityInvalidated(
                        public_key,
                        core::AuthCapability::Signer,
                        instance,
                    )));
                }
                if core.active_pubkey() == Some(public_key) {
                    effects.extend(core.handle(EngineMsg::SetActivePubkey(None)));
                }
                dispatch_core_effects(
                    &mut core,
                    effects,
                    &pool,
                    &mut row_channels,
                    &mut history_channels,
                    &mut diag_channels,
                    &registry,
                    dispatch_runtime,
                );
                let _ = reply.send(existed);
            }
            Cmd::ClearSession { reply } => {
                let removed = registry.clear();
                let mut effects = core.handle(EngineMsg::SetActivePubkey(None));
                for (public_key, instance) in removed {
                    auth_tasks.borrow_mut().cancel_capability(
                        public_key,
                        core::AuthCapability::Signer,
                        instance,
                    );
                    effects.extend(core.handle(EngineMsg::AuthCapabilityInvalidated(
                        public_key,
                        core::AuthCapability::Signer,
                        instance,
                    )));
                }
                dispatch_core_effects(
                    &mut core,
                    effects,
                    &pool,
                    &mut row_channels,
                    &mut history_channels,
                    &mut diag_channels,
                    &registry,
                    dispatch_runtime,
                );
                let _ = reply.send(());
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
                // The owner's one inward edge, spelled out: the selected
                // author is the reducer's one current-account copy (#1657),
                // the signing capability is the signer registry's, and
                // neither is reached through `RuntimeSessionState`'s `Deref`.
                // This read and the `Identity::Active` resolution inside the
                // reducer are now the same value, not two copies that a
                // missed assignment could split.
                active_sign_events.admit(
                    core.active_pubkey(),
                    registry.signer_registry(),
                    sign_event::CompletionWiring {
                        runtime: runtime_handle,
                        inbox: self_inbox,
                    },
                    unsigned,
                    completion,
                    &reply,
                );
            }
            Cmd::CancelSignEvent(id) => {
                active_sign_events.cancel(id);
            }
            Cmd::SignEventFinished(id) => {
                active_sign_events.finish(id);
            }
            Cmd::ObserveDiagnostics { reply } => {
                let id = next_diag_id;
                next_diag_id += 1;
                let (tx, rx) = latest_channel();
                // Same pool-count stitch as the `Effect::EmitDiagnostics` arm
                // (issue #121) — the proactive open-time snapshot must carry
                // the relay-cap rejection count too, not only the ones fanned
                // out later.
                seed_diagnostics_observer(
                    diagnostics_snapshot_with_pool(&core, &pool),
                    &tx,
                    &diag_channels,
                    &diagnostics_delivery,
                );
                if reply.send((id, rx)).is_err() {
                    // Caller already gave up -- nothing to register.
                    continue;
                }
                diag_channels.insert(id, tx);
            }
            Cmd::UnobserveDiagnostics(id) => {
                diag_channels.remove(&id);
                diagnostics_delivery
                    .borrow_mut()
                    .clear_if_unobserved(!diag_channels.is_empty());
            }
            Cmd::ReattachReceipt {
                id,
                cursor,
                sender,
                registration,
                reply,
            } => {
                let page = core.reattach_receipt_page(id, cursor, FACT_CHANNEL_CAPACITY);
                let found = deliver_receipt_replay_page(
                    &core,
                    &mut receipt_deliveries.borrow_mut(),
                    id,
                    sender,
                    registration,
                    page,
                );
                let _ = reply.send(found);
            }
            Cmd::DetachReceiptDelivery { id, registration } => {
                receipt_deliveries.borrow_mut().detach(id, &registration);
            }
            #[cfg(feature = "bench-instrumentation")]
            Cmd::ObservationOwnershipCensus { reply } => {
                let core_census = core.observation_ownership_census();
                let _ = reply.send(ObservationOwnershipCensus {
                    handles: core_census.handles,
                    histories: core_census.histories,
                    history_handles: core_census.history_handles,
                    resolver_nodes: core_census.resolver_nodes,
                    demand_atoms: core_census.demand_atoms,
                    planned_sessions: core_census.planned_sessions,
                    pending_execution_owners: core_census.pending_execution_owners,
                    active_execution_owners: core_census.active_execution_owners,
                    live_wire_owners: core_census.live_wire_owners,
                    row_channels: row_channels.len(),
                    history_channels: history_channels.len(),
                });
            }
            #[cfg(feature = "bench-instrumentation")]
            Cmd::DeadlineRaceProbe {
                entered, release, ..
            } => {
                let _ = entered.send(());
                let _ = release.recv();
            }
            Cmd::PublishQueueEntries {
                event_id,
                after,
                limit,
                reply,
            } => {
                let result = match event_id {
                    Some(event_id) => core.publish_queue_entries_for_event(event_id, after, limit),
                    None => core.publish_queue_entries(after, limit),
                };
                let _ =
                    reply.send(
                        result.map_err(|error| PublishQueueReadError::PersistenceFailed {
                            reason: error.to_string(),
                        }),
                    );
            }
            Cmd::RemovePublishQueueEntry { id, reply } => {
                let result = core.remove_publish_queue_entry(id);
                if result.is_ok() {
                    receipt_deliveries.borrow_mut().forget(id);
                }
                let _ = reply.send(result);
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
                    &registry,
                    dispatch_runtime,
                );
            }
            Cmd::PublishTracked {
                intent,
                sender,
                registration,
                reply,
            } => {
                let mut preparation = core.prepare_publish(intent);
                loop {
                    match preparation {
                        PublishPreparation::Complete(publish_effects) => {
                            complete_tracked_publish(
                                &mut core,
                                publish_effects,
                                sender,
                                registration,
                                reply,
                                &pool,
                                &mut row_channels,
                                &mut history_channels,
                                &mut diag_channels,
                                &registry,
                                dispatch_runtime,
                            );
                            break;
                        }
                        PublishPreparation::Materialize(prepared) => {
                            let core::PreparedReplaceableMaterialization { call, continuation } =
                                *prepared;
                            let outcome = core.run_replaceable_materialization(call);
                            preparation = core.complete_body_complete_replaceable_operation(
                                continuation,
                                outcome,
                            );
                        }
                    }
                }
            }
            Cmd::Subscribe { query, reply } => {
                let (id, seed, mut effects) = match core.open_observation(query, command_wall_now) {
                    ObservationOpen::Opened { id, seed, effects } => (id, seed, effects),
                    ObservationOpen::Refused { reason, effects } => {
                        let _ =
                            reply.send(Err(EngineThreadError::ObservationUnavailable { reason }));
                        dispatch_core_effects(
                            &mut core,
                            effects,
                            &pool,
                            &mut row_channels,
                            &mut history_channels,
                            &mut diag_channels,
                            &registry,
                            dispatch_runtime,
                        );
                        continue;
                    }
                };
                let (rows_tx, rows_rx) = rows_channel();
                row_channels.insert(id, rows_tx);
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
                        &registry,
                        dispatch_runtime,
                    );
                    continue;
                }
                effects.push(Effect::EmitRows(id, seed.deltas, seed.evidence));
                dispatch_core_effects(
                    &mut core,
                    effects,
                    &pool,
                    &mut row_channels,
                    &mut history_channels,
                    &mut diag_channels,
                    &registry,
                    dispatch_runtime,
                );
            }
            Cmd::SubscribeHistory { query, reply } => {
                let (id, seed, mut effects) =
                    match core.open_history_observation(query, command_wall_now) {
                        ObservationOpen::Opened { id, seed, effects } => (id, seed, effects),
                        ObservationOpen::Refused { reason, effects } => {
                            let _ = reply
                                .send(Err(EngineThreadError::ObservationUnavailable { reason }));
                            dispatch_core_effects(
                                &mut core,
                                effects,
                                &pool,
                                &mut row_channels,
                                &mut history_channels,
                                &mut diag_channels,
                                &registry,
                                dispatch_runtime,
                            );
                            continue;
                        }
                    };
                let (history_tx, history_rx) = latest_channel();
                history_channels.insert(id, history_tx);
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
                        &registry,
                        dispatch_runtime,
                    );
                    continue;
                }
                effects.push(Effect::EmitHistory(id, seed));
                dispatch_core_effects(
                    &mut core,
                    effects,
                    &pool,
                    &mut row_channels,
                    &mut history_channels,
                    &mut diag_channels,
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
                    if reply.send(Ok(())).is_err() {
                        let rollback = core.handle(EngineMsg::RollbackHistoryLoad(id));
                        dispatch_core_effects(
                            &mut core,
                            rollback,
                            &pool,
                            &mut row_channels,
                            &mut history_channels,
                            &mut diag_channels,
                            &registry,
                            dispatch_runtime,
                        );
                        continue;
                    }
                    // The staged turn's own effects, dispatched BEFORE the
                    // commit that follows them (#1886). They carry the wire
                    // admission arm the advance's REQs need and whatever the
                    // stage itself closed; discarding them on success -- while
                    // dispatching them on failure -- lost the first advance's
                    // REQ entirely. Order is the engine's decision order: the
                    // stage decided these before the commit below decided its
                    // supersede-closes, and the wire must see them that way.
                    dispatch_core_effects(
                        &mut core,
                        effects,
                        &pool,
                        &mut row_channels,
                        &mut history_channels,
                        &mut diag_channels,
                        &registry,
                        dispatch_runtime,
                    );
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
                        dispatch_core_effects(
                            &mut core,
                            committed,
                            &pool,
                            &mut row_channels,
                            &mut history_channels,
                            &mut diag_channels,
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
                    &registry,
                    dispatch_runtime,
                );
            }
            Cmd::RelayWorkerRetired => {
                retry_required_relay_workers(&core, &pool);
            }
            Cmd::Engine(EngineMsg::RelayFrame(handle, session, frame)) => {
                let effects = core.handle(EngineMsg::RelayFrame(handle, session, frame));
                dispatch_core_effects(
                    &mut core,
                    effects,
                    &pool,
                    &mut row_channels,
                    &mut history_channels,
                    &mut diag_channels,
                    &registry,
                    dispatch_runtime,
                );
            }
            Cmd::Engine(EngineMsg::Unsubscribe(id)) => {
                let effects = core.handle(EngineMsg::Unsubscribe(id));
                dispatch_core_effects(
                    &mut core,
                    effects,
                    &pool,
                    &mut row_channels,
                    &mut history_channels,
                    &mut diag_channels,
                    &registry,
                    dispatch_runtime,
                );
                // Deliver the terminal observation-scoped withdrawal fact
                // before dropping the sender; the app then observes channel
                // disconnect deterministically.
                row_channels.remove(&id);
            }
            Cmd::Engine(EngineMsg::SetActivePubkey(pk)) => {
                // P3: current identity is a reactive read input. Accepted
                // writes separately pin their exact author at acceptance.
                let effects = core.handle(EngineMsg::SetActivePubkey(pk));
                dispatch_core_effects(
                    &mut core,
                    effects,
                    &pool,
                    &mut row_channels,
                    &mut history_channels,
                    &mut diag_channels,
                    &registry,
                    dispatch_runtime,
                );
            }
            Cmd::Engine(EngineMsg::Publish(_)) => {
                unreachable!("runtime publishes always carry a fresh delivery target")
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
                    &registry,
                    dispatch_runtime,
                );
            }
        }
    }

    auth_tasks.borrow_mut().shutdown();
    registry.cancel_all_pending_writes();
    active_sign_events.drain_for_shutdown();

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
fn complete_tracked_publish(
    core: &mut EngineCore,
    mut publish_effects: Vec<Effect>,
    sender: FifoSender<WriteFact>,
    registration: ReceiptDeliveryRegistration,
    reply: Sender<Result<(ReceiptId, EventId), PublishError>>,
    pool: &Pool,
    row_channels: &mut HashMap<ObservationId, RowsSender>,
    history_channels: &mut HashMap<HistorySessionId, LatestSender<HistoryMsg>>,
    diag_channels: &mut HashMap<u64, LatestSender<DiagnosticsSnapshot>>,
    registry: &SignerRegistry,
    runtime: DispatchRuntime<'_>,
) {
    let result = publish_result(&publish_effects);
    let replay = take_publish_replay(&mut publish_effects);
    if let Ok((id, _)) = result {
        if let Some((replay_id, page)) = replay {
            debug_assert_eq!(replay_id, id);
            let (_, next_cursor) = deliver_receipt_replay_page(
                core,
                &mut runtime.receipt_deliveries.borrow_mut(),
                id,
                sender,
                registration.clone(),
                page,
            );
            debug_assert!(
                next_cursor.is_none(),
                "correlation publish replay is calculated as one complete page"
            );
        } else {
            runtime.receipt_deliveries.borrow_mut().register(
                id,
                registration.clone(),
                sender,
                ReceiptReplayCursor::new(id),
            );
        }
    }
    let accepted = result.as_ref().ok().map(|(id, _)| *id);
    if reply.send(result).is_err() {
        if let Some(id) = accepted {
            runtime
                .receipt_deliveries
                .borrow_mut()
                .detach(id, &registration);
        }
    }
    dispatch_core_effects(
        core,
        publish_effects,
        pool,
        row_channels,
        history_channels,
        diag_channels,
        registry,
        runtime,
    );
}

#[allow(clippy::too_many_arguments)]
fn reduce_and_dispatch_committed_observations(
    core: &mut EngineCore,
    frames: Vec<(nmp_transport::RelayHandle, RelaySessionKey, RelayFrame)>,
    pool: &Pool,
    row_channels: &mut HashMap<ObservationId, RowsSender>,
    history_channels: &mut HashMap<HistorySessionId, LatestSender<HistoryMsg>>,
    diag_channels: &mut HashMap<u64, LatestSender<DiagnosticsSnapshot>>,
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
            registry,
            runtime,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn reduce_and_dispatch_relay_frames(
    core: &mut EngineCore,
    frames: Vec<(nmp_transport::RelayHandle, RelaySessionKey, RelayFrame)>,
    pool: &Pool,
    row_channels: &mut HashMap<ObservationId, RowsSender>,
    history_channels: &mut HashMap<HistorySessionId, LatestSender<HistoryMsg>>,
    diag_channels: &mut HashMap<u64, LatestSender<DiagnosticsSnapshot>>,
    registry: &SignerRegistry,
    runtime: DispatchRuntime<'_>,
) {
    #[cfg(feature = "bench-instrumentation")]
    let phase_started = std::time::Instant::now();
    #[cfg(feature = "bench-instrumentation")]
    let cpu_started = nmp_engine::ingest_attribution::thread_cpu_time_ns();
    let effects = core.handle(EngineMsg::RelayFrames(frames));
    #[cfg(feature = "bench-instrumentation")]
    nmp_engine::ingest_attribution::relay_core_reduce(phase_started.elapsed());
    #[cfg(feature = "bench-instrumentation")]
    nmp_engine::ingest_attribution::relay_core_reduce_cpu(
        nmp_engine::ingest_attribution::thread_cpu_time_ns().saturating_sub(cpu_started),
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
        registry,
        runtime,
    );
    #[cfg(feature = "bench-instrumentation")]
    nmp_engine::ingest_attribution::relay_effect_dispatch(phase_started.elapsed());
}

/// Release workers no longer owned by the reducer, then execute its effects.
/// Terminal CLOSE frames for an obsolete session travel WITH its retirement
/// so the connected worker flushes them before socket teardown. Release must
/// otherwise happen first: when a cap-sized plan replaces every relay,
/// keeping the old workers through `apply_wire_delta` would make every new
/// `ensure_open` fail even though the new plan itself is within the cap.
/// `relay_worker_requirements` includes nonterminal durable/ephemeral write work,
/// so this cannot evict a worker merely because its last read REQ vanished.
// Deliberately mirrors `dispatch_effects`' reviewed runtime destinations and
// adds only the reducer reference needed for exact ownership reconciliation.
#[allow(clippy::too_many_arguments)]
fn dispatch_core_effects(
    core: &mut EngineCore,
    effects: Vec<Effect>,
    pool: &Pool,
    row_channels: &mut HashMap<ObservationId, RowsSender>,
    history_channels: &mut HashMap<HistorySessionId, LatestSender<HistoryMsg>>,
    diag_channels: &mut HashMap<u64, LatestSender<DiagnosticsSnapshot>>,
    registry: &SignerRegistry,
    runtime: DispatchRuntime<'_>,
) {
    {
        let required = core.relay_worker_requirements();
        let mut terminal_frames: BTreeMap<RelaySessionKey, Vec<String>> = BTreeMap::new();
        for effect in &effects {
            let Effect::Wire(delta) = effect else {
                continue;
            };
            for (session, ops) in &delta.ops {
                if required.all.contains(session) {
                    continue;
                }
                for op in ops {
                    if let WireOp::Close(sub_id) = op {
                        terminal_frames
                            .entry(session.clone())
                            .or_default()
                            .push(close_frame_text(sub_id));
                    }
                }
            }
        }
        for event in pool.close_unrequired_sessions(&required.all, terminal_frames) {
            if let Some(msg) = translate_pool_event(event) {
                let _ = runtime.self_inbox.send(Cmd::Engine(msg));
            }
        }
    }

    dispatch_effects(
        core,
        effects,
        pool,
        row_channels,
        history_channels,
        diag_channels,
        registry,
        runtime,
    );
}

#[allow(clippy::too_many_arguments)]
fn dispatch_relay_open_failure(
    core: &mut EngineCore,
    session: RelaySessionKey,
    error: nmp_transport::RelayOpenError,
    pool: &Pool,
    row_channels: &mut HashMap<ObservationId, RowsSender>,
    history_channels: &mut HashMap<HistorySessionId, LatestSender<HistoryMsg>>,
    diag_channels: &mut HashMap<u64, LatestSender<DiagnosticsSnapshot>>,
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

/// Open one reducer-required session for `Effect::EnsureWriteRelay`.
///
/// `max_relays` is a physical-session ceiling (#8), so a Public read and a
/// `Nip42(author)` write to the same URL cannot coexist at `max_relays = 1`.
/// The old behavior let the already-live Public worker win forever: every
/// write retry was cap-refused while nothing ever released the read owner,
/// leaving a durable receipt parked at `AwaitingRelay` (#598).
///
/// A protected write is a durable obligation, so it may time-share the SAME
/// relay's one slot by releasing that relay's Public worker first. The
/// reducer still owns both demands: its Public read plan remains current, the
/// synchronous disconnect fact is fed back through the ordinary engine
/// inbox, and once the write lane becomes terminal exact worker
/// reconciliation retires the protected worker. The ensuing
/// `RelayWorkerRetired` retry restores the still-required session bound to no identity,
/// whose Connected transition replays the plan once.
///
/// This never exceeds the configured worker/thread envelope, never merges
/// access contexts onto one socket, and never evicts a different relay.
fn ensure_write_effect_session(
    session: &RelaySessionKey,
    pool: &Pool,
    self_inbox: &Sender<Cmd>,
) -> Result<nmp_transport::RelayHandle, nmp_transport::RelayOpenError> {
    match pool.ensure_session(session) {
        Ok(handle) => Ok(handle),
        Err(nmp_transport::RelayOpenError::AtCapacity { .. })
            if session.authenticate_as.is_some() =>
        {
            let unauthenticated = RelaySessionKey::unauthenticated(session.relay.clone());
            let Some(unauthenticated_handle) = pool.live_session_handle(&unauthenticated) else {
                return pool.ensure_session(session);
            };
            if let Some(event) = pool.close(unauthenticated_handle) {
                if let Some(message) = translate_pool_event(event) {
                    let _ = self_inbox.send(Cmd::Engine(message));
                }
            }
            pool.ensure_session(session)
        }
        Err(error) => Err(error),
    }
}

/// Retry the exact currently-owned relay-session set once after an actual
/// worker join releases retirement capacity. The ordinary Connected path
/// advances public reads; protected reads park until the exact AUTH OK.
/// Every NMP read worker keeps an empty transport preamble because reducer
/// replay is the single generation-aware owner.
fn retry_required_relay_workers(core: &EngineCore, pool: &Pool) {
    let required = core.relay_worker_requirements();
    // #598: a protected durable obligation may have released a same-relay
    // Public worker to time-share a cap-sized pool. When the retirement slot
    // becomes reusable, restore the protected session first; reopening Public
    // first would consume the slot again and recreate the permanent
    // AwaitingRelay stall. Once the write becomes terminal, it leaves
    // `required` and the still-owned session bound to no identity is the next retry.
    let mut all: Vec<_> = required.all.into_iter().collect();
    all.sort_by(|left, right| {
        let left_write = required.writes.contains(left);
        let right_write = required.writes.contains(right);
        right_write.cmp(&left_write).then_with(|| left.cmp(right))
    });
    for session in all {
        if pool.live_session_handle(&session).is_some() {
            continue;
        }
        let Ok(handle) = pool.ensure_session(&session) else {
            continue;
        };
        pool.set_reconnect_preamble(handle, Vec::new());
    }
}

/// Execute every `Effect` `EngineCore::handle` returned, in order.
// Deliberately spells out each reviewed runtime destination so effect routing
// cannot acquire hidden mutable state.
#[allow(clippy::too_many_arguments)]
fn dispatch_effects(
    core: &mut EngineCore,
    effects: Vec<Effect>,
    pool: &Pool,
    row_channels: &mut HashMap<ObservationId, RowsSender>,
    history_channels: &mut HashMap<HistorySessionId, LatestSender<HistoryMsg>>,
    diag_channels: &mut HashMap<u64, LatestSender<DiagnosticsSnapshot>>,
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
            registry,
            runtime,
        );
    }
    runtime.receipt_deliveries.borrow_mut().finish_batch(core);
}

/// Re-root the author-route provider onto the authors the reducer now needs.
///
/// The LOOP owns every reducer call here. `open_observation` is the same door
/// `Cmd::Subscribe` uses and it RETURNS the id, so nothing reconstructs a
/// handle by scanning emitted effects. The provider is never told the id at
/// all; it can neither mint one nor keep one.
///
/// A provider may also answer immediately — a static table, an app-managed
/// cache — which is why `reroot` returns updates beside its instruction.
fn provider_reroot(
    core: &mut EngineCore,
    slot: &mut RouteProviderSlot,
    needs: BTreeSet<PublicKey>,
) -> Vec<Effect> {
    // A panicking provider is refused exactly like one that re-roots onto
    // silence: `Closed` drops the current observation and opens nothing
    // (see `ProviderReroot`), and `guarded_provider_call` has already
    // poisoned the slot so it is never called again. See #1802.
    let (reroot, updates) = guarded_provider_call(slot, |provider| provider.reroot(needs))
        .unwrap_or((ProviderReroot::Closed, Vec::new()));
    let mut effects = Vec::new();
    if !matches!(reroot, ProviderReroot::Unchanged) {
        // Both remaining outcomes close the current observation. Only
        // `Reopened` opens a replacement -- which is why `ProviderReroot` is
        // not an `Option`.
        if let Some(handle) = slot.bound.take() {
            effects.extend(core.handle(EngineMsg::Unsubscribe(handle)));
        }
        if let ProviderReroot::Reopened(query) = reroot {
            let now = core.clock();
            match core.open_observation(query, now) {
                ObservationOpen::Opened {
                    id,
                    seed,
                    effects: opened,
                } => {
                    slot.bound = Some(id);
                    effects.extend(opened);
                    effects.push(Effect::EmitRows(id, seed.deltas, seed.evidence));
                }
                // A refused provider query leaves the slot unbound, exactly
                // as the previous path did when no handle could be found.
                ObservationOpen::Refused {
                    effects: refused, ..
                } => effects.extend(refused),
            }
        }
    }
    effects.extend(apply_author_routes(core, updates));
    effects
}

/// Apply a provider's neutral replacements through the one author-route
/// writer. `AuthorRouteReplacement` is the reducer's own vocabulary; the
/// provider states the fact, the reducer decides what it means.
fn apply_author_routes(
    core: &mut EngineCore,
    updates: Vec<core::AuthorRouteUpdate>,
) -> Vec<Effect> {
    let mut effects = Vec::new();
    for update in updates {
        core.replace_author_routes(update.author, update.replacement, &mut effects);
    }
    effects
}

// Deliberately mirrors `dispatch_effects`; each destination remains explicit
// at the one-effect boundary where its ownership is audited.
#[allow(clippy::too_many_arguments)]
fn dispatch_effect(
    core: &mut EngineCore,
    effect: Effect,
    pool: &Pool,
    row_channels: &mut HashMap<ObservationId, RowsSender>,
    history_channels: &mut HashMap<HistorySessionId, LatestSender<HistoryMsg>>,
    diag_channels: &mut HashMap<u64, LatestSender<DiagnosticsSnapshot>>,
    registry: &SignerRegistry,
    runtime: DispatchRuntime<'_>,
) {
    match effect {
        Effect::ArmWireAdmission => {
            runtime.wire_admission.borrow_mut().arm(Instant::now());
        }
        Effect::AuthorRouteNeedsChanged(needs) => {
            // No provider: the need is dropped and every author stays
            // `Unknown`. Operator lanes and explicit routes are unaffected,
            // and nothing anywhere converts the silence into a verdict.
            let followups = {
                let mut slot = runtime.route_provider.borrow_mut();
                slot.as_mut()
                    .map(|slot| provider_reroot(core, slot, needs))
                    .unwrap_or_default()
            };
            dispatch_effects(
                core,
                followups,
                pool,
                row_channels,
                history_channels,
                diag_channels,
                registry,
                runtime,
            );
        }
        Effect::UpdateCommittedObservations {
            invalidated,
            published,
        } => {
            #[cfg(feature = "bench-instrumentation")]
            let phase_started = std::time::Instant::now();
            pool.update_committed_observations(invalidated, published);
            #[cfg(feature = "bench-instrumentation")]
            nmp_engine::ingest_attribution::committed_observation_effect(phase_started.elapsed());
        }
        Effect::Wire(delta) => {
            let outcomes = apply_wire_delta(&delta, pool);
            for outcome in outcomes {
                let evidence = core.on_wire_request_handoff(outcome);
                dispatch_effects(
                    core,
                    evidence,
                    pool,
                    row_channels,
                    history_channels,
                    diag_channels,
                    registry,
                    runtime,
                );
            }
        }
        Effect::Replay(session, reqs) => {
            let outcomes = apply_replay(&session, &reqs, pool);
            for outcome in outcomes {
                let evidence = core.on_wire_request_handoff(outcome);
                dispatch_effects(
                    core,
                    evidence,
                    pool,
                    row_channels,
                    history_channels,
                    diag_channels,
                    registry,
                    runtime,
                );
            }
        }
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
        Effect::EnsureReadRelay(session) => {
            // Read ownership cannot displace an already-live physical
            // session. A typed cap refusal remains observable in pool
            // diagnostics and is reconciled after a real worker retirement.
            if let Err(error) = pool.ensure_session(&session) {
                dispatch_relay_open_failure(
                    core,
                    session,
                    error,
                    pool,
                    row_channels,
                    history_channels,
                    diag_channels,
                    registry,
                    runtime,
                );
            }
        }
        Effect::EnsureWriteRelay(session) => {
            // The durable lane is already persisted as WaitingConnection.
            // A typed cap refusal remains observable in pool diagnostics and
            // must not be converted back into an invalid handle or a busy
            // retry loop here. A protected write may, however, time-share
            // this relay's already-live Public slot (#598).
            if let Err(error) = ensure_write_effect_session(&session, pool, runtime.self_inbox) {
                dispatch_relay_open_failure(
                    core,
                    session,
                    error,
                    pool,
                    row_channels,
                    history_channels,
                    diag_channels,
                    registry,
                    runtime,
                );
            }
        }
        // The signer frozen into this exact accepted template is looked up
        // by pubkey on every request. A later current-account switch cannot
        // redirect outstanding work. No matching registered signer is
        // NOT a terminal signer failure. The accepted pending row and
        // obligation stay alive as `AwaitingCapability`; only an explicit
        // denial/error from an attached signer compensates the write.
        Effect::RequestSign(id, generation, unsigned) => match registry.sign(unsigned) {
            Some(operation) => match operation {
                SignerOp::Ready(result) => {
                    let result = result.and_then(decode_signed_event);
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
                        let result = pending.await.and_then(decode_signed_event);
                        let _ = inbox.send(Cmd::Engine(EngineMsg::SignerCompleted(
                            id, generation, result,
                        )));
                    });
                }
            },
            None => {
                let effects = core.handle(EngineMsg::SignerUnavailable(id, generation));
                dispatch_core_effects(
                    core,
                    effects,
                    pool,
                    row_channels,
                    history_channels,
                    diag_channels,
                    registry,
                    runtime,
                );
            }
        },
        Effect::RelayAuth(effect) => {
            let mut bind = |token, capability, instance| {
                let effects = core.handle(EngineMsg::AuthCapabilityBound {
                    token,
                    capability,
                    instance,
                });
                // `on_auth_capability_bound` itself always returns an empty
                // vec, but `EngineCore::handle`'s epilogue can append to
                // *any* arm's result afterward -- in particular
                // `prune_unowned_relay_state`, which is live exactly when
                // `auth_required_sessions` is non-empty, the state this
                // message fires in. So "this call produces no effects" is
                // not a property this call site can assume; dispatch
                // whatever comes back, the same way every other
                // `core.handle` call site in this file does. See #1803.
                dispatch_core_effects(
                    core,
                    effects,
                    pool,
                    row_channels,
                    history_channels,
                    diag_channels,
                    registry,
                    runtime,
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
        Effect::EmitRows(id, rows, evidence) => {
            let provider_updates = {
                let mut slot = runtime.route_provider.borrow_mut();
                slot.as_mut()
                    .filter(|slot| slot.bound == Some(id))
                    .and_then(|slot| {
                        guarded_provider_call(slot, |provider| provider.observe_rows(&rows))
                    })
            };
            if let Some(updates) = provider_updates {
                let followups = apply_author_routes(core, updates);
                dispatch_effects(
                    core,
                    followups,
                    pool,
                    row_channels,
                    history_channels,
                    diag_channels,
                    registry,
                    runtime,
                );
                return;
            }
            if let Some(tx) = row_channels.get(&id) {
                tx.send((rows, evidence, Vec::new()));
            }
        }
        Effect::EmitObservationEvidence(id, evidence) => {
            let provider_updates = {
                let mut slot = runtime.route_provider.borrow_mut();
                slot.as_mut()
                    .filter(|slot| slot.bound == Some(id))
                    .and_then(|slot| {
                        guarded_provider_call(slot, |provider| provider.observe_evidence(&evidence))
                    })
            };
            if let Some(updates) = provider_updates {
                let followups = apply_author_routes(core, updates);
                dispatch_effects(
                    core,
                    followups,
                    pool,
                    row_channels,
                    history_channels,
                    diag_channels,
                    registry,
                    runtime,
                );
                return;
            }
            if let Some(tx) = row_channels.get(&id) {
                tx.send_evidence(evidence);
            }
        }
        Effect::EmitHistory(id, batch) => {
            if let Some(tx) = history_channels.get(&id) {
                #[cfg(feature = "bench-instrumentation")]
                let send_started = std::time::Instant::now();
                tx.send(batch);
                #[cfg(feature = "bench-instrumentation")]
                nmp_engine::ingest_attribution::history_channel_send(send_started.elapsed());
            }
        }
        Effect::HistoryLoadResult(..) => {}
        Effect::DiagnosticsChanged => {
            runtime
                .diagnostics_delivery
                .borrow_mut()
                .changed(Instant::now(), !diag_channels.is_empty());
        }
        Effect::EmitDiagnostics(mut snapshot) => {
            #[cfg(feature = "bench-instrumentation")]
            let phase_started = std::time::Instant::now();
            // This full snapshot is at least as current as any pending lazy
            // marker. Satisfy that cohort so its deadline cannot duplicate
            // this proactive delivery.
            runtime.diagnostics_delivery.borrow_mut().satisfy();
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
            fan_out_diagnostics(snapshot, diag_channels);
            #[cfg(feature = "bench-instrumentation")]
            nmp_engine::ingest_attribution::diagnostics_effect(phase_started.elapsed());
        }
        Effect::EmitReceipt(id, status) => {
            if matches!(
                &status,
                WriteFact::Signing(SigningState::Signed { .. })
                    | WriteFact::Outcome(WriteOutcome::NotSent(_))
                    | WriteFact::Outcome(WriteOutcome::Superseded)
            ) {
                registry.cancel_pending_write(id);
            }
            runtime
                .receipt_deliveries
                .borrow_mut()
                .deliver(core, id, status);
        }
        Effect::ReplayReceipt(..) => {
            unreachable!("publish replay must be consumed with its fresh delivery target")
        }
        Effect::WriteAccepted(..) => {
            // Custody, not a fact: `publish()` returning `Ok` already said
            // it, so nothing fans this out to an observer.
        }
        Effect::PublishFailed(..) => {
            // `PublishTracked` consumes this typed pre-receipt failure for
            // its synchronous reply. There is no receipt stream to fan out.
        }
    }
}

/// The app-facing handle to a live diagnostics stream (returned by
/// [`Handle::observe_diagnostics`]). Withdraw it via [`Self::cancel`] when
/// the caller is done; unlike [`QueryHandle`] there is no `Drop` teardown
/// HERE (this value carries no resource of its own beyond the registry
/// entry it names) — a diagnostics handle above the facade is what ties
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
/// vocabulary preserves guarantee #2/#3 at the top edge. M4 §5 added signer
/// registration to close the multi-account gap; M5 added read-only
/// diagnostics; #464 adds governed sign-only without creating a third
/// workload noun or bypassing the current-account signing-provider boundary:
///
/// - `subscribe(LiveQuery) -> (QueryHandle, RowsReceiver)`
/// - `unsubscribe(QueryHandle)`
/// - `add_signer(impl SigningCapability) -> Result<SignerRegistration, AddSignerError>`
/// - `remove_signer(SignerRegistration) -> bool`
/// - `sign_event(UnsignedEvent) -> SignEventOperation`
/// - `set_current_account(Option<PublicKey>)`
/// - `publish(WriteIntent) -> Receiver<WriteFact>`
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

/// Benchmark-only hold at the deterministic command/deadline race boundary.
///
/// While this value is alive the engine has processed every deadline due at
/// the probed instant and is blocked before executing the synthetic command.
/// Dropping it releases the engine. This is mechanism instrumentation, not an
/// application API.
#[cfg(feature = "bench-instrumentation")]
#[doc(hidden)]
pub struct DeadlineRaceHold {
    release: Option<Sender<()>>,
}

#[cfg(feature = "bench-instrumentation")]
impl Drop for DeadlineRaceHold {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
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
///
/// #827: the facade's own retention falsifier used to reach this across the
/// `nmp-engine` crate boundary, which is why the feature exists at all. The
/// gate is the same `#[cfg(feature = "test-instrumentation")]` spelling the
/// NIP-11 service itself uses; production builds are unchanged.
#[cfg(feature = "test-instrumentation")]
#[doc(hidden)]
pub fn relay_information_retention_census(
    handle: &Handle,
) -> nmp_nip11::RelayInformationRetentionCensus {
    handle.relay_information.retention_census()
}

impl Handle {
    pub fn session_snapshot(&self) -> Option<SessionSnapshot> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::SessionSnapshot { reply: reply_tx })
            .ok()?;
        reply_rx.recv().ok()
    }

    pub fn session_export_sources(&self) -> Option<RuntimeSessionExportSources> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::SessionExportSources { reply: reply_tx })
            .ok()?;
        reply_rx.recv().ok()
    }

    pub fn current_session_pubkey(&self) -> Option<Option<PublicKey>> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::CurrentSessionPubkey { reply: reply_tx })
            .ok()?;
        reply_rx.recv().ok()
    }

    pub fn add_private_key_account(
        &self,
        signer: nmp_local_signer::LocalKeySigner,
        make_current: bool,
    ) -> Result<SessionAccount, AddSignerError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::AddPrivateKeyAccount {
                signer,
                make_current,
                reply: reply_tx,
            })
            .map_err(|_| AddSignerError::EngineShuttingDown)?;
        reply_rx
            .recv()
            .unwrap_or(Err(AddSignerError::EngineShuttingDown))
    }

    pub fn add_public_key_account(
        &self,
        public_key: PublicKey,
        make_current: bool,
    ) -> Option<SessionAccount> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::AddPublicKeyAccount {
                public_key,
                make_current,
                reply: reply_tx,
            })
            .ok()?;
        reply_rx.recv().ok()
    }

    pub fn make_current_account(&self, public_key: PublicKey) -> Option<bool> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::MakeCurrentAccount {
                public_key,
                reply: reply_tx,
            })
            .ok()?;
        reply_rx.recv().ok()
    }

    pub fn remove_session_account(&self, public_key: PublicKey) -> Option<bool> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::RemoveSessionAccount {
                public_key,
                reply: reply_tx,
            })
            .ok()?;
        reply_rx.recv().ok()
    }

    pub fn clear_session(&self) -> Option<()> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::ClearSession { reply: reply_tx })
            .ok()?;
        reply_rx.recv().ok()
    }

    /// Acquire NIP-11 once through the engine-owned cache. This may block
    /// the CALLER on HTTP, never the reducer thread. The resolved
    /// advertisement is also fed back into capability decision-making.
    pub fn relay_information(
        &self,
        relay: RelayUrl,
        policy: RelayInformationCachePolicy,
    ) -> Result<RelayInformationSnapshot, RelayInformationError> {
        let snapshot = self.relay_information.get(relay.clone(), policy)?;
        let information = nip11::capability_evidence(&snapshot);
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
        let information = nip11::capability_evidence(&snapshot);
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
    /// `HandleId` and the row channel is registered, then returns both. #704
    /// (review): a relay whose initially-required connection worker cannot be
    /// opened — including a rare OS thread-spawn refusal — is NOT a subscription
    /// failure; that relay is reported as unavailable in acquisition evidence
    /// and the subscription proceeds on its other sources. A canonical
    /// store/resolver failure before the first frame instead returns
    /// [`EngineThreadError::ObservationUnavailable`] with no live handle.
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
    /// accepted (or was a no-op / `AtBound` beat); `Some(Err(_))` when the
    /// canonical store could not stage the advance.
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
    /// current — call [`Self::set_current_account`] to actually switch reads
    /// and writes onto it. Blocks briefly (one engine-thread round trip,
    /// same discipline as [`Self::subscribe`]) and returns an opaque scoped
    /// registration. The registration exposes the key and is the only value
    /// that may later detach this exact installation.
    ///
    /// # Panics
    /// If the engine thread has already shut down.
    pub fn add_signer<Sig>(&self, signer: Sig) -> Result<SignerRegistration, AddSignerError>
    where
        Sig: SigningCapability + Send + Sync + 'static,
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

    /// Re-root every reactive query and default unsigned-publish authority
    /// onto `pk` (or onto none). Accepted writes are not redirected: each
    /// resolves the signer identity frozen at its acceptance boundary.
    /// `pk` need not already be registered via [`Self::add_signer`] — e.g.
    /// read-only browsing of an account this app holds no key for is legal. Publishing
    /// resolves the signer pinned by the draft's own author; if none is
    /// registered, the accepted intent remains `AwaitingCapability`.
    pub fn set_current_account(&self, pk: Option<PublicKey>) {
        let _ = self.inbox.send(Cmd::Engine(EngineMsg::SetActivePubkey(pk)));
    }

    /// Read the app's own publish queue back (#1039).
    ///
    /// Inspection, never waiting: this returns what NMP knows right now and
    /// never blocks on settlement.
    pub fn publish_queue_entries(
        &self,
        after: Option<ReceiptId>,
        limit: u8,
    ) -> Result<Vec<PublishQueueEntry>, PublishQueueReadError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::PublishQueueEntries {
                event_id: None,
                after,
                limit,
                reply: reply_tx,
            })
            .map_err(|_| PublishQueueReadError::EngineClosed)?;
        reply_rx
            .recv()
            .map_err(|_| PublishQueueReadError::EngineClosed)?
    }

    /// Read one bounded page of currently open obligations for `event_id`.
    pub fn publish_queue_entries_for_event(
        &self,
        event_id: EventId,
        after: Option<ReceiptId>,
        limit: u8,
    ) -> Result<Vec<PublishQueueEntry>, PublishQueueReadError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::PublishQueueEntries {
                event_id: Some(event_id),
                after,
                limit,
                reply: reply_tx,
            })
            .map_err(|_| PublishQueueReadError::EngineClosed)?;
        reply_rx
            .recv()
            .map_err(|_| PublishQueueReadError::EngineClosed)?
    }

    /// Forget one queue entry (#1039). How a write parked forever on a
    /// missing signer, or a permanently-failed refused entry, ever ends —
    /// the parked one through [`Self::cancel_write`] first, which ends the
    /// obligation and compensates the optimistic row, leaving the terminal
    /// receipt this door then forgets. An entry whose obligation is still
    /// open is refused.
    pub fn remove_publish_queue_entry(&self, id: ReceiptId) -> Result<(), RemoveQueueEntryError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::RemovePublishQueueEntry {
                id,
                reply: reply_tx,
            })
            .map_err(|_| RemoveQueueEntryError::EngineClosed)?;
        reply_rx
            .recv()
            .map_err(|_| RemoveQueueEntryError::EngineClosed)?
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
    /// Delivers the CURRENT snapshot immediately. Proactive full diagnostic
    /// effects remain immediate; lazy change markers are coalesced into one
    /// latest full snapshot within a first-change-anchored 16 ms bound —
    /// pushed reactively, never polled (D8). Delivery stays latest-wins if the
    /// consumer is slow (see `diagnostics_channel`'s doc — no unbounded
    /// backlog, no dropped row-equivalent data since this is a recomputed
    /// projection, not a delta stream). Blocks briefly (one engine-thread
    /// round trip, same discipline as [`Self::subscribe`]/[`Self::add_signer`]).
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

    #[cfg(feature = "bench-instrumentation")]
    #[doc(hidden)]
    pub fn observation_ownership_census(&self) -> ObservationOwnershipCensus {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::ObservationOwnershipCensus { reply: reply_tx })
            .expect("nmp-engine: observation ownership census called after shutdown");
        reply_rx
            .recv()
            .expect("nmp-engine: engine dropped observation ownership census reply")
    }

    /// Make one synthetic command ready at exactly `at`, then hold it after
    /// the runtime has executed any core deadline due at that same instant.
    #[cfg(feature = "bench-instrumentation")]
    #[doc(hidden)]
    pub fn bench_hold_due_deadline_command(&self, at: Timestamp) -> DeadlineRaceHold {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::DeadlineRaceProbe {
                at,
                entered: entered_tx,
                release: release_rx,
            })
            .expect("nmp-engine: deadline race probe called after shutdown");
        entered_rx
            .recv()
            .expect("nmp-engine: deadline race probe was not entered");
        DeadlineRaceHold {
            release: Some(release_tx),
        }
    }

    /// Stop the engine thread (and, transitively, its bridge threads — see
    /// [`EngineThread::join`]). Idempotent: a `Handle` clone calling this
    /// after another already has just finds the inbox gone and no-ops.
    pub fn shutdown(&self) {
        let _ = self.inbox.send(Cmd::Shutdown);
    }
}
