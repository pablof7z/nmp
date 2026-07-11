//! The async edge (plan §2 position 2). `EngineThread` spawns TWO dedicated
//! OS threads:
//!
//! - the **engine thread**, which owns `core::EngineCore` and runs a
//!   blocking `mpsc::Receiver<Cmd>::recv()` loop (D8: blocking recv, never
//!   poll) — for every command it calls `EngineCore::handle`/`::tick` and
//!   dispatches the returned `core::Effect`s to `nmp_transport::Pool::send`,
//!   the `nmp_signer` capability, and the app-facing channels;
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
//! on_rows` carries only `Vec<RowDelta>` (no coverage), while `Effect::
//! EmitRows` carries `(HandleId, Vec<RowDelta>, QueryCoverage)` — the
//! query-level coverage the M3 ruling makes part of the read contract (test
//! 9's headline). This runtime therefore picks ONE channel per plan's
//! guidance: rows+coverage are delivered from `Effect::EmitRows` alone (via
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

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use nmp_grammar::ConcreteFilter;
use nmp_resolver::{HandleId, LiveQuery};
use nmp_router::{RelayDirectory, SubId, WireDelta, WireOp, WireReq};
use nmp_signer::{SignerError, SignerOp, SigningCapability};
use nmp_store::EventStore;
use nostr::{ClientMessage, JsonUtil, PublicKey, RelayUrl, SubscriptionId};

use nmp_transport::{Pool, PoolConfig, PoolEvent, WireFrame};

use crate::core::{self, Effect, EngineCore, EngineMsg, QueryCoverage, RowDelta, RowSink};
use crate::outbox::{ReceiptSink, WriteIntent, WriteStatus};

/// One delivered batch for a live subscription: raw rows + the query's
/// aggregate coverage (see the module doc's "two delivery channels" note).
pub type RowsMsg = (Vec<RowDelta>, QueryCoverage);

/// The app-facing handle to a live subscription (returned by
/// [`Handle::subscribe`]). `Send`, `Copy`-cheap, carries nothing that
/// borrows into the engine thread — it is exactly the correlation id
/// [`Handle::unsubscribe`] needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryHandle(HandleId);

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
    /// Register a new signing capability (M4 §5: `SignerRegistry`). The
    /// reply carries the pubkey the engine thread's registry keyed it under
    /// (`None` if `signer.public_key()` itself returned `None` — there is no
    /// key to register it against, so it is dropped rather than stored
    /// unreachably).
    AddSigner {
        signer: Box<dyn SigningCapability + Send>,
        reply: Sender<Option<PublicKey>>,
    },
    Shutdown,
}

/// Every signing capability the engine thread currently holds, keyed by its
/// own public key, plus which one currently backs `Effect::RequestSign` (M4
/// §5: closes the known multi-account gap). `Handle::set_active_account` is
/// the ONE app-facing verb that moves both this registry's `active` pointer
/// AND `EngineCore`'s read root together (P3: identity is one input — see
/// the `Cmd::Engine(EngineMsg::SetActivePubkey(..))` arm in `engine_loop`),
/// so reads and writes can never independently point at different accounts.
#[derive(Default)]
struct SignerRegistry {
    signers: HashMap<PublicKey, Box<dyn SigningCapability + Send>>,
    active: Option<PublicKey>,
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

    /// Move the active pointer. `None` is a legal, deliberate state — a
    /// logged-out / read-only session (M4 §5: "the engine may start with
    /// zero accounts").
    fn set_active(&mut self, pk: Option<PublicKey>) {
        self.active = pk;
    }

    /// The capability currently backing `Effect::RequestSign`, if any is
    /// both set active AND still registered.
    fn active_signer(&self) -> Option<&(dyn SigningCapability + Send)> {
        let pk = self.active?;
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
    let mut preambles: Preambles = Preambles::new();
    let mut registry = SignerRegistry::default();

    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            Cmd::Shutdown => break,
            Cmd::AddSigner { signer, reply } => {
                let pk = registry.add(signer);
                let _ = reply.send(pk);
            }
            Cmd::Subscribe { query, reply } => {
                let effects = core.handle(EngineMsg::Subscribe(query, Box::new(NullRowSink)));
                // `on_subscribe` always emits exactly one `Effect::EmitRows`
                // for the handle it just created (its `last_coverage` starts
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
                    &mut preambles,
                    &registry,
                    self_inbox,
                );
            }
            Cmd::Engine(EngineMsg::SetActivePubkey(pk)) => {
                // P3, M4 §5: the read root and the active signing capability
                // move TOGETHER here, so `Handle::set_active_account` (this
                // command's app-facing name) can never leave them pointing
                // at different accounts.
                registry.set_active(pk);
                let effects = core.handle(EngineMsg::SetActivePubkey(pk));
                dispatch_effects(
                    effects,
                    &pool,
                    &mut row_channels,
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
    preambles: &mut Preambles,
    registry: &SignerRegistry,
    self_inbox: &Sender<Cmd>,
) {
    for effect in effects {
        dispatch_effect(effect, pool, row_channels, preambles, registry, self_inbox);
    }
}

fn dispatch_effect(
    effect: Effect,
    pool: &Pool,
    row_channels: &mut HashMap<HandleId, Sender<RowsMsg>>,
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
        // M4 §5: the active signer is looked up fresh on every `RequestSign`
        // (never cached at spawn time) so a `set_active_account` switch is
        // observed by the very next publish. No active/registered signer is
        // NOT a panic -- it is fed through the identical `SignerCompleted`
        // completion path a real signer failure would take, so
        // `EngineCore::on_signer_completed`'s existing `Err` arm (untouched)
        // is what turns it into `WriteStatus::Failed`, exactly as if the
        // signer itself had rejected the op.
        Effect::RequestSign(id, unsigned) => match registry.active_signer() {
            Some(signer) => match signer.sign(unsigned) {
                SignerOp::Ready(result) => {
                    let _ = self_inbox.send(Cmd::Engine(EngineMsg::SignerCompleted(id, result)));
                }
                SignerOp::Pending(rx) => {
                    // A single blocking recv on a fresh thread, then exactly
                    // one forwarded message -- D8-compliant (no poll loop),
                    // and keeps the engine thread itself from ever blocking
                    // on a remote signer round-trip.
                    let inbox = self_inbox.clone();
                    thread::spawn(move || {
                        if let Ok(result) = rx.recv() {
                            let _ = inbox.send(Cmd::Engine(EngineMsg::SignerCompleted(id, result)));
                        }
                    });
                }
            },
            None => {
                let _ = self_inbox.send(Cmd::Engine(EngineMsg::SignerCompleted(
                    id,
                    Err(SignerError::Unavailable),
                )));
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
        Effect::EmitRows(id, rows, coverage) => {
            if let Some(tx) = row_channels.get(&id) {
                let _ = tx.send((rows, coverage));
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

/// The cheap, `Clone + Send` app-facing handle. Exactly five verbs plus
/// `shutdown` (ledger #2/#3 preserved at the top edge — plan §5 test 14
/// grep-guards this structural property; M4 §5 adds `add_signer` to close
/// the multi-account gap, the one deliberate widening of this surface):
///
/// - `subscribe(LiveQuery) -> (QueryHandle, Receiver<RowsMsg>)`
/// - `unsubscribe(QueryHandle)`
/// - `add_signer(impl SigningCapability) -> Option<PublicKey>`
/// - `set_active_account(Option<PublicKey>)`
/// - `publish(WriteIntent) -> Receiver<WriteStatus>`
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

    /// Re-root every reactive query AND the active signing capability
    /// together onto `pk` (or onto neither, for `None`) — P3: identity is a
    /// pure input, never ambient, and M4 §5's structural fix for the
    /// known account-switching gap: one verb moves both halves so reads and
    /// writes can never diverge onto different accounts. `pk` need not
    /// already be registered via [`Self::add_signer`] — e.g. read-only
    /// browsing of an account this app holds no key for is legal; any
    /// `publish` attempted while active in that state simply terminates
    /// `WriteStatus::Failed` (no active signer), never a panic.
    pub fn set_active_account(&self, pk: Option<PublicKey>) {
        let _ = self.inbox.send(Cmd::Engine(EngineMsg::SetActivePubkey(pk)));
    }

    /// Enqueue a write. Fire-and-forget: the returned `Receiver` streams
    /// every `WriteStatus` this intent ever reaches (ledger #9 — enqueue is
    /// not converged; the FIRST value is never a terminal for a durable/
    /// at-most-once intent, and an `Ephemeral` intent's receiver simply
    /// never yields anything).
    #[must_use]
    pub fn publish(&self, intent: WriteIntent) -> Receiver<WriteStatus> {
        let (tx, rx) = mpsc::channel();
        let sink: Box<dyn ReceiptSink> = Box::new(ChannelReceiptSink(tx));
        let _ = self
            .inbox
            .send(Cmd::Engine(EngineMsg::Publish(intent, sink)));
        rx
    }

    /// Stop the engine thread (and, transitively, the pool-bridge thread —
    /// see [`EngineThread::join`]). Idempotent: a `Handle` clone calling this
    /// after another already has just finds the inbox gone and no-ops.
    pub fn shutdown(&self) {
        let _ = self.inbox.send(Cmd::Shutdown);
    }
}
