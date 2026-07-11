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
//! for a relay on every call ("last call wins" — see that method's doc).
//! `EngineCore`'s `Effect::Wire`/`Effect::Replay` are deltas/snapshots of the
//! CURRENT demand, not the preamble text itself, so this module keeps its
//! own per-relay `SubId -> wire REQ text` map (`Preambles`) and re-derives
//! the full preamble string list on every touch — see `apply_wire_delta`/
//! `apply_replay`.

mod diagnostics_channel;

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use nmp_grammar::ConcreteFilter;
use nmp_resolver::{HandleId, LiveQuery};
use nmp_router::{RelayDirectory, SubId, WireDelta, WireOp, WireReq};
use nmp_signer::{SignerOp, SigningCapability};
use nmp_store::EventStore;
use nostr::{ClientMessage, JsonUtil, PublicKey, RelayUrl, SubscriptionId, Timestamp};

use nmp_transport::{Pool, PoolConfig, PoolEvent, WireFrame};

use crate::core::{
    self, AcquisitionEvidence, DiagnosticsSnapshot, Effect, EngineCore, EngineMsg, ReceiptId,
    RowDelta, RowSink,
};
use crate::outbox::{ReceiptSink, WriteIntent, WriteStatus};

pub use diagnostics_channel::LatestReceiver;
use diagnostics_channel::{latest_channel, LatestSender};

/// One delivered batch for a live subscription: raw rows + the query's
/// per-source acquisition evidence (see the module doc's "two delivery
/// channels" note).
pub type RowsMsg = (Vec<RowDelta>, AcquisitionEvidence);

/// The app-facing handle to a live subscription (returned by
/// [`Handle::subscribe`]). `Send`, `Copy`-cheap, carries nothing that
/// borrows into the engine thread — it is exactly the correlation id
/// [`Handle::unsubscribe`] needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryHandle(HandleId);

/// A newly accepted write's stable store-issued identity plus its live
/// observer. Keeping the id separate from the channel lets a later process
/// call [`Handle::reattach_receipt`] without replaying acceptance.
pub struct ReceiptStream {
    pub id: ReceiptId,
    pub statuses: Receiver<WriteStatus>,
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
    Subscribe {
        query: LiveQuery,
        reply: Sender<(HandleId, Receiver<RowsMsg>)>,
    },
    PublishTracked {
        intent: WriteIntent,
        sink: Box<dyn ReceiptSink>,
        reply: Sender<ReceiptId>,
    },
    ReattachReceipt {
        id: ReceiptId,
        sink: Box<dyn ReceiptSink>,
        reply: Sender<bool>,
    },
    /// Register a new signing capability (M4 §5: `SignerRegistry`). The
    /// reply carries the pubkey the engine thread's registry keyed it under
    /// (`None` if `signer.public_key()` itself returned `None` — there is no
    /// key to register it against, so it is dropped rather than stored
    /// unreachably).
    AddSigner {
        signer: Box<dyn SigningCapability + Send>,
        reply: Sender<Option<PublicKey>>,
    },
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
    signers: HashMap<PublicKey, Box<dyn SigningCapability + Send>>,
}

impl SignerRegistry {
    /// Register `signer` under its own `public_key()`, replacing any prior
    /// capability already registered for that key. Returns the key it was
    /// registered under (`None` if the capability reports no key at all).
    fn add(&mut self, signer: Box<dyn SigningCapability + Send>) -> Option<PublicKey> {
        let pk = signer.public_key();
        if let Some(pk) = pk {
            self.signers.insert(pk, signer);
        }
        pk
    }

    /// Resolve the signer frozen into this exact accepted template. An
    /// account switch cannot redirect already-accepted work.
    fn signer_for(&self, pk: PublicKey) -> Option<&(dyn SigningCapability + Send)> {
        self.signers.get(&pk).map(AsRef::as_ref)
    }
}

/// One dedicated engine OS thread (§2 position 2) plus the pool-bridge
/// thread that feeds it. Returned alongside the [`Handle`] the app actually
/// uses; kept around only so a caller (chiefly tests) can deterministically
/// `join` both threads after triggering [`Handle::shutdown`].
pub struct EngineThread {
    engine_join: Option<JoinHandle<()>>,
    bridge_join: Option<JoinHandle<()>>,
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
    #[must_use]
    pub fn spawn<S, D>(
        store: S,
        directory: D,
        cap: usize,
        pool_config: PoolConfig,
    ) -> (Self, Handle)
    where
        S: EventStore + Send + 'static,
        D: RelayDirectory + Send + 'static,
    {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let (pool_evt_tx, pool_evt_rx) = mpsc::channel::<PoolEvent>();
        // The pool's OWN mio worker threads + translator thread are interior
        // to `Pool` (harvested, HARVEST-justified in nmp-transport's own
        // docs) — this crate never touches mio/tungstenite directly.
        let pool = Pool::new(pool_config, pool_evt_tx);

        let bridge_inbox = cmd_tx.clone();
        let bridge_join = thread::Builder::new()
            .name("nmp-engine-pool-bridge".to_string())
            .spawn(move || pool_bridge_loop(&pool_evt_rx, &bridge_inbox))
            .expect("nmp-engine: pool-bridge thread spawn must succeed");

        let self_inbox = cmd_tx.clone();
        let engine_join = thread::Builder::new()
            .name("nmp-engine".to_string())
            .spawn(move || engine_loop(store, directory, cap, pool, &cmd_rx, &self_inbox))
            .expect("nmp-engine: engine thread spawn must succeed");

        (
            Self {
                engine_join: Some(engine_join),
                bridge_join: Some(bridge_join),
            },
            Handle { inbox: cmd_tx },
        )
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
fn pool_bridge_loop(pool_evt_rx: &Receiver<PoolEvent>, engine_inbox: &Sender<Cmd>) {
    while let Ok(event) = pool_evt_rx.recv() {
        if let Some(msg) = translate_pool_event(event) {
            if engine_inbox.send(Cmd::Engine(msg)).is_err() {
                break; // engine thread is gone; nothing left to feed.
            }
        }
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
/// `PoolEvent::Health` has no `EngineMsg` counterpart in M3 (diagnostics
/// only, no reducer vocabulary consumes it yet) and is dropped.
fn translate_pool_event(event: PoolEvent) -> Option<EngineMsg> {
    match event {
        PoolEvent::Connected { handle, url } => Some(EngineMsg::RelayConnected(handle, url)),
        PoolEvent::Disconnected { slot, .. } => Some(EngineMsg::RelayDisconnected(slot)),
        PoolEvent::Frame { handle, frame } => Some(EngineMsg::RelayFrame(handle, frame)),
        PoolEvent::Health { .. } => None,
    }
}

/// Per-relay reconnect-preamble bookkeeping: the full set of currently-live
/// REQ wire texts, keyed by `SubId` so `WireOp::Req`/`Close` can update it
/// incrementally (module doc: `Pool::set_reconnect_preamble` replaces the
/// WHOLE preamble on every call, so this module must always hand it the
/// complete current set, not a delta).
type Preambles = HashMap<RelayUrl, HashMap<SubId, String>>;

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
    pool: Pool,
    cmd_rx: &Receiver<Cmd>,
    self_inbox: &Sender<Cmd>,
) where
    S: EventStore,
    D: RelayDirectory + 'static,
{
    let mut core = EngineCore::new(store, Box::new(directory), cap);
    let mut row_channels: HashMap<HandleId, Sender<RowsMsg>> = HashMap::new();
    let mut diag_channels: HashMap<u64, LatestSender<DiagnosticsSnapshot>> = HashMap::new();
    let mut next_diag_id: u64 = 0;
    let mut preambles: Preambles = Preambles::new();
    let mut registry = SignerRegistry::default();

    // Recovery happens before the first externally-issued command. Pending
    // rows already live in the store; this only rebuilds ownership and may
    // replay exact durable attempt bytes whose Started fact was committed.
    let recovery_effects = core.recover_on_boot();
    dispatch_effects(
        recovery_effects,
        &pool,
        &mut row_channels,
        &mut diag_channels,
        &mut preambles,
        &registry,
        self_inbox,
    );

    loop {
        let cmd = match core.next_deadline() {
            None => match cmd_rx.recv() {
                Ok(cmd) => cmd,
                Err(_) => break, // every `Sender` (incl. `self_inbox`) is gone.
            },
            Some(deadline) => match cmd_rx.recv_timeout(duration_until(deadline, Timestamp::now()))
            {
                Ok(cmd) => cmd,
                Err(RecvTimeoutError::Timeout) => {
                    // Woke EXACTLY at the deadline (or it was already past,
                    // e.g. boot-time catch-up on a persisted index) -- fire
                    // the mechanism, then re-arm from the NEW next_deadline
                    // rather than acting on the one that just fired.
                    let effects = core.handle(EngineMsg::Tick(Timestamp::now()));
                    dispatch_effects(
                        effects,
                        &pool,
                        &mut row_channels,
                        &mut diag_channels,
                        &mut preambles,
                        &registry,
                        self_inbox,
                    );
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            },
        };
        match cmd {
            Cmd::Shutdown => break,
            Cmd::AddSigner { signer, reply } => {
                let pk = registry.add(signer);
                let _ = reply.send(pk);
                if let Some(pk) = pk {
                    let effects = core.handle(EngineMsg::SignerAttached(pk));
                    dispatch_effects(
                        effects,
                        &pool,
                        &mut row_channels,
                        &mut diag_channels,
                        &mut preambles,
                        &registry,
                        self_inbox,
                    );
                }
            }
            Cmd::ObserveDiagnostics { reply } => {
                let id = next_diag_id;
                next_diag_id += 1;
                let (tx, rx) = latest_channel();
                tx.send(core.diagnostics_snapshot());
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
                effects.extend(core.handle(EngineMsg::Publish(intent, sink)));
                let id = effects
                    .iter()
                    .find_map(|effect| match effect {
                        Effect::EmitReceipt(id, _) => Some(*id),
                        _ => None,
                    })
                    .expect("every publish produces a receipt correlation id");
                let _ = reply.send(id);
                dispatch_effects(
                    effects,
                    &pool,
                    &mut row_channels,
                    &mut diag_channels,
                    &mut preambles,
                    &registry,
                    self_inbox,
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
                if reply.send((id, rows_rx)).is_err() {
                    // Caller already gave up on `subscribe()` -- withdraw
                    // immediately rather than leak a live demand atom nobody
                    // will ever read from.
                    row_channels.remove(&id);
                    let _ = core.handle(EngineMsg::Unsubscribe(id));
                    continue;
                }
                dispatch_effects(
                    effects,
                    &pool,
                    &mut row_channels,
                    &mut diag_channels,
                    &mut preambles,
                    &registry,
                    self_inbox,
                );
            }
            Cmd::Engine(EngineMsg::Unsubscribe(id)) => {
                let effects = core.handle(EngineMsg::Unsubscribe(id));
                // Drop the sender: the app's `Receiver` observes disconnect.
                row_channels.remove(&id);
                dispatch_effects(
                    effects,
                    &pool,
                    &mut row_channels,
                    &mut diag_channels,
                    &mut preambles,
                    &registry,
                    self_inbox,
                );
            }
            Cmd::Engine(EngineMsg::SetActivePubkey(pk)) => {
                // P3: active identity is a reactive read input. Accepted
                // writes separately pin their exact author at acceptance.
                let effects = core.handle(EngineMsg::SetActivePubkey(pk));
                dispatch_effects(
                    effects,
                    &pool,
                    &mut row_channels,
                    &mut diag_channels,
                    &mut preambles,
                    &registry,
                    self_inbox,
                );
            }
            Cmd::Engine(EngineMsg::Publish(intent, sink)) => {
                // Acceptance timestamps and NIP-40 refusal are wall-clock
                // facts. Advance the pure reducer clock immediately before
                // the one accept transaction; otherwise a fresh runtime's
                // clock would remain zero until its first unrelated deadline.
                let mut effects = core.handle(EngineMsg::Tick(Timestamp::now()));
                effects.extend(core.handle(EngineMsg::Publish(intent, sink)));
                dispatch_effects(
                    effects,
                    &pool,
                    &mut row_channels,
                    &mut diag_channels,
                    &mut preambles,
                    &registry,
                    self_inbox,
                );
            }
            Cmd::Engine(msg) => {
                let effects = core.handle(msg);
                dispatch_effects(
                    effects,
                    &pool,
                    &mut row_channels,
                    &mut diag_channels,
                    &mut preambles,
                    &registry,
                    self_inbox,
                );
            }
        }
    }

    // Tear down this thread's OWN `Pool` clone. If no other `Pool` clone
    // survives (the design here never keeps one anywhere else), this drops
    // the last `Arc<PoolInner>` reference after `shutdown` runs, which in
    // turn drops the pool's sink -- the very thing `EngineThread::join`'s
    // doc explains lets the bridge thread's `recv` finally disconnect.
    pool.shutdown();
}

/// Execute every `Effect` `EngineCore::handle` returned, in order.
fn dispatch_effects(
    effects: Vec<Effect>,
    pool: &Pool,
    row_channels: &mut HashMap<HandleId, Sender<RowsMsg>>,
    diag_channels: &mut HashMap<u64, LatestSender<DiagnosticsSnapshot>>,
    preambles: &mut Preambles,
    registry: &SignerRegistry,
    self_inbox: &Sender<Cmd>,
) {
    for effect in effects {
        dispatch_effect(
            effect,
            pool,
            row_channels,
            diag_channels,
            preambles,
            registry,
            self_inbox,
        );
    }
}

fn dispatch_effect(
    effect: Effect,
    pool: &Pool,
    row_channels: &mut HashMap<HandleId, Sender<RowsMsg>>,
    diag_channels: &mut HashMap<u64, LatestSender<DiagnosticsSnapshot>>,
    preambles: &mut Preambles,
    registry: &SignerRegistry,
    self_inbox: &Sender<Cmd>,
) {
    match effect {
        Effect::Wire(delta) => apply_wire_delta(&delta, pool, preambles),
        Effect::Replay(url, reqs) => apply_replay(&url, reqs, pool, preambles),
        Effect::PublishEvent(url, event) => {
            let handle = pool.ensure_open(&url);
            let json = ClientMessage::event(event).as_json();
            let _ = pool.send(handle, WireFrame::Text(json));
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
                    let _ = self_inbox.send(Cmd::Engine(EngineMsg::SignerCompleted(
                        id, generation, result,
                    )));
                }
                SignerOp::Pending(rx) => {
                    // A single blocking recv on a fresh thread, then exactly
                    // one forwarded message -- D8-compliant (no poll loop),
                    // and keeps the engine thread itself from ever blocking
                    // on a remote signer round-trip.
                    let inbox = self_inbox.clone();
                    thread::spawn(move || {
                        if let Ok(result) = rx.recv() {
                            let _ = inbox.send(Cmd::Engine(EngineMsg::SignerCompleted(
                                id, generation, result,
                            )));
                        }
                    });
                }
            },
            None => {
                let _ = self_inbox.send(Cmd::Engine(EngineMsg::SignerUnavailable(id, generation)));
            }
        },
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
        Effect::EmitDiagnostics(snapshot) => {
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
        Effect::StartProbe(url, sub_id, filter, initial_hex) => {
            let handle = pool.ensure_open(&url);
            let text = neg_open_frame_text(&sub_id, &filter, initial_hex);
            let _ = pool.send(handle, WireFrame::Text(text));
        }
        Effect::NegOpen(probed, sub_id, filter, initial_hex) => {
            let relay = probed.url().clone();
            let handle = pool.ensure_open(&relay);
            let text = neg_open_frame_text(&sub_id, &filter, initial_hex);
            let _ = pool.send(handle, WireFrame::Text(text));
        }
        Effect::NegMsg(relay, sub_id, message_hex) => {
            let handle = pool.ensure_open(&relay);
            let text = neg_msg_frame_text(&sub_id, message_hex);
            let _ = pool.send(handle, WireFrame::Text(text));
        }
        Effect::NegClose(relay, sub_id) => {
            let handle = pool.ensure_open(&relay);
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

/// `Effect::Wire`'s per-relay ops -> wire frames + reconnect-preamble
/// upkeep. `ensure_open` is idempotent for an already-live slot (ships the
/// frame onto whichever generation is current, queuing it if the socket is
/// still dialing) and transparently reopens a previously-closed one, so
/// there is no separate "is this relay already open" bookkeeping to keep
/// here.
fn apply_wire_delta(delta: &WireDelta, pool: &Pool, preambles: &mut Preambles) {
    for (relay, ops) in &delta.ops {
        let handle = pool.ensure_open(relay);
        let entry = preambles.entry(relay.clone()).or_default();
        for op in ops {
            match op {
                WireOp::Req(sub_id, filter) => {
                    let text = req_frame_text(sub_id, filter);
                    let _ = pool.send(handle, WireFrame::Text(text.clone()));
                    entry.insert(sub_id.clone(), text);
                }
                WireOp::Close(sub_id) => {
                    let text = close_frame_text(sub_id);
                    let _ = pool.send(handle, WireFrame::Text(text));
                    entry.remove(sub_id);
                }
            }
        }
        let frames: Vec<String> = entry.values().cloned().collect();
        pool.set_reconnect_preamble(handle, frames);
    }
}

/// `Effect::Replay`: `reqs` is `EngineCore`'s full CURRENT req list for
/// `url` at the moment it observed `RelayConnected` (`core/mod.rs`'s
/// `on_relay_connected`) -- an authoritative snapshot, not a delta, so the
/// preamble entry for this relay is rebuilt from scratch rather than
/// patched. Resending these as fresh REQ frames on the just-connected handle
/// is what makes reconnection replay observable even on the very first
/// `Connected` for a relay (before any preamble could have existed yet); on
/// a later automatic reconnect the pool's own preamble mechanism will
/// typically have already replayed them, and resending here is a harmless,
/// idempotent overwrite (NIP-01: a REQ with an existing sub-id replaces that
/// sub).
fn apply_replay(url: &RelayUrl, reqs: Vec<WireReq>, pool: &Pool, preambles: &mut Preambles) {
    let handle = pool.ensure_open(url);
    let entry = preambles.entry(url.clone()).or_default();
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

/// The cheap, `Clone + Send` app-facing handle. Exactly five verbs plus
/// `shutdown` (ledger #2/#3 preserved at the top edge — plan §5 test 14
/// grep-guards this structural property; M4 §5 adds `add_signer` to close
/// the multi-account gap; M5 adds `observe_diagnostics`, the one other
/// deliberate widening — read-only, off the data path, never influences
/// routing/delivery):
///
/// - `subscribe(LiveQuery) -> (QueryHandle, Receiver<RowsMsg>)`
/// - `unsubscribe(QueryHandle)`
/// - `add_signer(impl SigningCapability) -> Option<PublicKey>`
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
}

impl Handle {
    /// Open a live subscription. Blocks (briefly — one engine-thread round
    /// trip, never network-bound) until `EngineCore` has assigned the
    /// `HandleId` and the row channel is registered, then returns both.
    ///
    /// # Panics
    /// If the engine thread has already shut down. Calling `subscribe`
    /// after `shutdown` is a caller bug, not a recoverable runtime state —
    /// there is no engine left to own the subscription.
    #[must_use]
    pub fn subscribe(&self, query: LiveQuery) -> (QueryHandle, Receiver<RowsMsg>) {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::Subscribe {
                query,
                reply: reply_tx,
            })
            .expect("nmp-engine: subscribe() called after the engine thread shut down");
        let (id, rows_rx) = reply_rx
            .recv()
            .expect("nmp-engine: engine thread dropped the subscribe reply");
        (QueryHandle(id), rows_rx)
    }

    /// Withdraw a live subscription. Fire-and-forget: once the engine thread
    /// processes it, the row channel's `Sender` is dropped and the app's
    /// `Receiver` observes a clean disconnect.
    pub fn unsubscribe(&self, handle: QueryHandle) {
        let _ = self
            .inbox
            .send(Cmd::Engine(EngineMsg::Unsubscribe(handle.0)));
    }

    /// Register a signing/crypto capability, keyed by its own `public_key()`
    /// (M4 §5: `SignerRegistry`). Registering a signer does NOT make it
    /// active — call [`Self::set_active_account`] to actually switch reads
    /// and writes onto it. Blocks briefly (one engine-thread round trip,
    /// same discipline as [`Self::subscribe`]) and returns the key it was
    /// registered under, or `None` if the capability reported no key at all.
    ///
    /// # Panics
    /// If the engine thread has already shut down.
    pub fn add_signer<Sig>(&self, signer: Sig) -> Option<PublicKey>
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
    /// no durable delivery obligation or query-visible pending row.
    #[must_use]
    pub fn publish(&self, intent: WriteIntent) -> Receiver<WriteStatus> {
        self.publish_tracked(intent).statuses
    }

    /// Enqueue a write and expose its stable receipt id. This synchronous
    /// round trip waits only for the local crash-atomic acceptance door,
    /// never for signing, routing, network I/O, or ACKs.
    #[must_use]
    pub fn publish_tracked(&self, intent: WriteIntent) -> ReceiptStream {
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
            .expect("nmp-engine: engine dropped publish receipt reply");
        ReceiptStream { id, statuses: rx }
    }

    /// Attach an additional observer to a retained receipt. The returned
    /// channel is primed with durable receipt/attempt facts; `None` means
    /// the id was never issued by this store.
    pub fn reattach_receipt(&self, id: ReceiptId) -> Option<Receiver<WriteStatus>> {
        let (tx, rx) = mpsc::channel();
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::ReattachReceipt {
                id,
                sink: Box::new(ChannelReceiptSink(tx)),
                reply: reply_tx,
            })
            .ok()?;
        reply_rx.recv().ok()?.then_some(rx)
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
