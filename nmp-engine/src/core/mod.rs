//! The PURE synchronous reducer (plan §2 position 1, §3.4). `EngineCore`
//! owns the M1 resolver `Engine<S>`, the M2 `Router`, the write-outbox
//! state, and the coverage-attribution bookkeeping (`attribution.rs`,
//! `coverage_query.rs`). Its entire surface is:
//!
//! ```ignore
//! impl<S: EventStore> EngineCore<S> {
//!     pub fn handle(&mut self, msg: EngineMsg) -> Vec<Effect>;
//!     pub fn tick(&mut self, now: nostr::Timestamp) -> Vec<Effect>;
//! }
//! ```
//!
//! `EngineCore` does NO I/O, spawns no threads, touches no socket, imposes
//! no runtime — this is the seam that preserves M1/M2's headless property:
//! the whole engine's logic is testable by feeding `EngineMsg`s and
//! asserting `Effect`s, with zero network (plan §5 tier A).
//!
//! Coverage attribution implements
//! `docs/consults/2026-07-11-fable-coverage-attribution.md` (the ruling)
//! EXACTLY: send-time snapshots + the FIFO intersection rule live in
//! [`attribution`]; the per-query `CompleteUpTo`/`Unknown` aggregation
//! lives in [`coverage_query`]. Both are engine-owned per the ruling — the
//! store (`nmp-store`) only stores whatever interval it is handed.

mod attribution;
mod coverage_query;

use std::collections::{BTreeMap, BTreeSet, HashMap};

use nostr::{
    Event as SignedEvent, EventId, JsonUtil, PublicKey, RelayMessage, RelayUrl, Timestamp,
    UnsignedEvent,
};

use nmp_grammar::ConcreteFilter;
use nmp_resolver::{Engine as ResolverEngine, HandleId, LiveQuery, QueryHandle};
use nmp_router::{
    DiscoveryKinds, RelayDirectory, RelayLimits, Router, RuleRegistry, WireDelta, WireOp, WireReq,
};
use nmp_signer::SignerError;
use nmp_store::{EventStore, RelayObserved};
use nmp_transport::{RelayFrame, RelayHandle as TransportRelayHandle};

use crate::negentropy::ProbedRelay;
use crate::outbox::{
    Durability, ReceiptSink, WriteIntent, WritePayload, WriteRouting, WriteStatus,
};

use attribution::AttributionState;
pub use coverage_query::QueryCoverage;
// `runtime` (C) needs the EXACT same wire subscription-id string
// `attribution.rs` records at send time (`AttributionState::record_send`) so
// that a REQ actually placed on the wire under this string round-trips back
// to the right `SubId` when the relay echoes it in an EOSE — re-derive it or
// drift silently breaks coverage attribution. `pub(crate)` (not a wider
// re-export): this is an internal wire-format detail `core` and `runtime`
// share, never a public contract for callers outside this crate.
pub(crate) use attribution::wire_sub_id_string;

/// Opaque id correlating a `Publish`/`RequestSign` to its `EmitReceipt`/
/// `SignerCompleted`.
#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq, PartialOrd, Ord)]
pub struct ReceiptId(pub u64);

/// Sink an app-facing `Handle` registers for row deltas on a subscription.
pub trait RowSink: Send {
    fn on_rows(&self, rows: Vec<RowDelta>);
}

/// A raw row delta (plan §7 non-goal: no ordering/windowing in M3 — raw
/// deltas + coverage only).
#[derive(Debug, Clone)]
pub struct RowDelta {
    pub event: nostr::Event,
}

/// The read/write/frame vocabulary the reducer consumes (plan §3.4).
pub enum EngineMsg {
    Subscribe(LiveQuery, Box<dyn RowSink>),
    Unsubscribe(HandleId),
    SetActivePubkey(Option<PublicKey>),
    Publish(WriteIntent, Box<dyn ReceiptSink>),
    RelayConnected(TransportRelayHandle, RelayUrl),
    RelayDisconnected(u32),
    RelayFrame(TransportRelayHandle, RelayFrame),
    SignerCompleted(ReceiptId, Result<SignedEvent, SignerError>),
    Tick(Timestamp),
}

/// The row/wire/receipt vocabulary the reducer emits (plan §3.4). `EmitRows`
/// carries the query-level [`QueryCoverage`] alongside its rows (ruling §6:
/// coverage is a property of the WHOLE query, aggregated across every atom's
/// covering relay set — not a per-row fact).
#[derive(Debug)]
pub enum Effect {
    /// -> `Pool::send` per (relay, current handle).
    Wire(WireDelta),
    /// Reconnect: resend the current wire subs on the NEW generation.
    Replay(RelayUrl, Vec<WireReq>),
    StartProbe(RelayUrl),
    NegOpen(ProbedRelay, ConcreteFilter),
    /// One per attributed atom per EOSE/NEG-DONE (ruling §7): the narrow
    /// atom's `CoverageKey`, the relay that proved it, and the proven
    /// interval.
    RecordCoverage(
        nmp_store::CoverageKey,
        RelayUrl,
        nmp_store::CoverageInterval,
    ),
    EmitRows(HandleId, Vec<RowDelta>, QueryCoverage),
    EmitReceipt(ReceiptId, WriteStatus),
    RequestSign(ReceiptId, UnsignedEvent),
    RequestDecrypt(EventId, PublicKey, String),
    /// Outbox: publish `event` to `relay` (plan §3.4's "`Effect::Wire`
    /// publish REQ/EVENT per relay", re-cut as its OWN effect rather than a
    /// `nmp_router::WireOp` variant — `WireOp`/`WireDelta` are read-
    /// subscription vocabulary owned by `nmp-router`, out of this builder's
    /// scope to extend; this is engine-owned wire vocabulary for the write
    /// plane). C (runtime) translates this to `Pool::send` of an `["EVENT",
    /// …]` frame on `relay`'s current generation.
    PublishEvent(RelayUrl, SignedEvent),
}

/// Per-handle bookkeeping `EngineCore` must retain across `handle()` calls:
/// the `QueryHandle` itself (dropping it would withdraw the subscription —
/// see `nmp_resolver::QueryHandle`'s `Drop` impl), the app-facing sink, and
/// the last-emitted row-id/coverage pair (so `EmitRows` fires only when
/// something actually changed, not on every unrelated recompile).
struct HandleState {
    _handle: QueryHandle,
    sink: Box<dyn RowSink>,
    last_rows: BTreeSet<EventId>,
    last_coverage: Option<QueryCoverage>,
}

/// Per-receipt bookkeeping the reducer retains from `Publish` through to the
/// last per-relay ack (or `Ephemeral`'s immediate forget). `sink: None`
/// marks an `Ephemeral` intent (ledger #9: "no receipt/ack" — the reducer
/// never again touches `sink`/emits `EmitReceipt` for this id once this is
/// `None`).
struct PendingWrite {
    durability: Durability,
    routing: WriteRouting,
    sink: Option<Box<dyn ReceiptSink>>,
    /// Set once the signer resolves; used to clean up `event_to_receipt`.
    event_id: Option<EventId>,
    /// Relays sent-to but not yet terminal (acked/rejected/given-up).
    /// Durable and AtMostOnce both populate this (both track real per-relay
    /// state); AtMostOnce's distinguishing property is that NOTHING in this
    /// reducer ever re-sends on a `RelayDisconnected` for either class — a
    /// dropped pending relay always resolves to `GaveUp`, never a retry
    /// `PublishEvent` (no blind retry, ledger's `AtMostOnce` amendment).
    pending_relays: BTreeSet<RelayUrl>,
}

/// The PURE synchronous reducer (§2 position 1). No I/O, no threads.
pub struct EngineCore<S: EventStore> {
    resolver: ResolverEngine<S>,
    router: Router,
    directory: Box<dyn RelayDirectory>,
    cap: usize,
    handles: HashMap<HandleId, HandleState>,
    attribution: AttributionState,
    /// `PoolEvent::Connected`/`Disconnected` carry a bare slot number, not a
    /// `RelayUrl` — this is EngineCore's own memory of which URL currently
    /// occupies which pool slot, populated on `RelayConnected`.
    slot_to_url: HashMap<u32, RelayUrl>,
    clock: Timestamp,
    next_receipt: u64,
    /// Write outbox (§3.4 / VISION §7 ledger #6/#9). `pending` is keyed by
    /// `ReceiptId` from `Publish` through to the last terminal per-relay
    /// status; `event_to_receipt` lets an inbound `OK` frame (keyed by
    /// `EventId` on the wire) find its receipt.
    pending: HashMap<ReceiptId, PendingWrite>,
    event_to_receipt: HashMap<EventId, ReceiptId>,
}

impl<S: EventStore> EngineCore<S> {
    pub fn new(store: S, directory: Box<dyn RelayDirectory>, cap: usize) -> Self {
        Self {
            resolver: ResolverEngine::new(store),
            router: Router::new(
                RelayLimits::default(),
                DiscoveryKinds::default(),
                RuleRegistry::default_widen_only(),
            ),
            directory,
            cap,
            handles: HashMap::new(),
            attribution: AttributionState::new(),
            slot_to_url: HashMap::new(),
            clock: Timestamp::from(0u64),
            next_receipt: 0,
            pending: HashMap::new(),
            event_to_receipt: HashMap::new(),
        }
    }

    /// Read-only access to the resolver's current demand (test/diagnostic
    /// convenience — the whole point of a headlessly-testable reducer is
    /// that its state can be inspected directly).
    pub fn active_demand(&self) -> BTreeSet<ConcreteFilter> {
        self.resolver.active_demand()
    }

    /// Read-only coverage introspection (test/diagnostic convenience,
    /// mirroring `active_demand`): the proven interval for `atom`'s
    /// window-erased shape at `relay`, if any coverage has been recorded.
    pub fn get_coverage(
        &self,
        atom: &ConcreteFilter,
        relay: &RelayUrl,
    ) -> Option<nmp_store::CoverageInterval> {
        self.resolver
            .store()
            .get_coverage(nmp_store::coverage_key(atom), relay)
    }

    pub fn tick(&mut self, now: Timestamp) -> Vec<Effect> {
        self.clock = now;
        // Backoff/keepalive/prober scheduling is D/E territory (depends on
        // A2/A3, not built in B); B's tick is a pure clock update.
        Vec::new()
    }

    pub fn handle(&mut self, msg: EngineMsg) -> Vec<Effect> {
        match msg {
            EngineMsg::Subscribe(query, sink) => self.on_subscribe(query, sink),
            EngineMsg::Unsubscribe(id) => self.on_unsubscribe(id),
            EngineMsg::SetActivePubkey(pk) => self.on_set_active_pubkey(pk),
            EngineMsg::Publish(intent, sink) => self.on_publish(intent, sink),
            EngineMsg::RelayConnected(handle, url) => self.on_relay_connected(handle, url),
            EngineMsg::RelayDisconnected(slot) => self.on_relay_disconnected(slot),
            EngineMsg::RelayFrame(handle, frame) => self.on_relay_frame(handle, frame),
            EngineMsg::SignerCompleted(id, result) => self.on_signer_completed(id, result),
            EngineMsg::Tick(now) => self.tick(now),
        }
    }

    // ---- subscribe / unsubscribe / re-root ------------------------------

    fn on_subscribe(&mut self, query: LiveQuery, sink: Box<dyn RowSink>) -> Vec<Effect> {
        let (qh, _delta) = self.resolver.subscribe(query);
        let id = qh.id();
        let mut effects = Vec::new();
        self.recompile(&mut effects);
        self.handles.insert(
            id,
            HandleState {
                _handle: qh,
                sink,
                last_rows: BTreeSet::new(),
                last_coverage: None,
            },
        );
        self.refresh_handle(id, &mut effects);
        effects
    }

    fn on_unsubscribe(&mut self, id: HandleId) -> Vec<Effect> {
        let _delta = self.resolver.unsubscribe(id);
        self.handles.remove(&id);
        let mut effects = Vec::new();
        self.recompile(&mut effects);
        effects
    }

    fn on_set_active_pubkey(&mut self, pk: Option<PublicKey>) -> Vec<Effect> {
        let _delta = self.resolver.set_active_pubkey(pk);
        let mut effects = Vec::new();
        self.recompile(&mut effects);
        self.refresh_all_handles(&mut effects);
        effects
    }

    // ---- write outbox (D: intent -> signed -> routed -> sent -> acked) --

    /// `Publish` (plan §3.4 step 1): accept durably, assign a `ReceiptId`,
    /// emit `RequestSign` unless the payload already carries a signature
    /// (VISION P: signing and publishing are orthogonal). `Ephemeral`
    /// intents never get a `sink`/`EmitReceipt` at all (ledger #9's
    /// amendment: "no receipt/ack" for ephemeral) — everything past this
    /// point (routing, wire) still runs for them, fire-and-forget.
    fn on_publish(&mut self, intent: WriteIntent, sink: Box<dyn ReceiptSink>) -> Vec<Effect> {
        let id = self.alloc_receipt_id();
        let WriteIntent {
            payload,
            durability,
            routing,
        } = intent;

        let sink = match durability {
            Durability::Ephemeral => None,
            _ => Some(sink),
        };

        let mut effects = Vec::new();
        if let Some(sink) = &sink {
            sink.on_status(WriteStatus::Accepted);
            effects.push(Effect::EmitReceipt(id, WriteStatus::Accepted));
        }

        self.pending.insert(
            id,
            PendingWrite {
                durability,
                routing,
                sink,
                event_id: None,
                pending_relays: BTreeSet::new(),
            },
        );

        match payload {
            WritePayload::Unsigned(unsigned) => {
                effects.push(Effect::RequestSign(id, unsigned));
            }
            WritePayload::Signed(event) => {
                self.on_signed(id, event, &mut effects);
            }
        }
        effects
    }

    /// `SignerCompleted` (plan §3.4 step 2 continuation): the runtime's
    /// signer capability resolved. `Err` is a whole-intent terminal
    /// (`WriteStatus::Failed`) — no relay was ever contacted.
    fn on_signer_completed(
        &mut self,
        id: ReceiptId,
        result: Result<SignedEvent, SignerError>,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        match result {
            Ok(event) => self.on_signed(id, event, &mut effects),
            Err(err) => {
                if let Some(pending) = self.pending.remove(&id) {
                    let status = WriteStatus::Failed(err.to_string());
                    if let Some(sink) = pending.sink {
                        sink.on_status(status.clone());
                        effects.push(Effect::EmitReceipt(id, status));
                    }
                }
            }
        }
        effects
    }

    /// Shared by the pre-signed (`on_publish`) and signer-completed paths:
    /// `Signed` -> resolve `WriteRouting` -> `Routed` -> `PublishEvent` per
    /// relay -> `Sent` per relay. Route failure (ledger #6) is a whole-
    /// intent `Failed` with NO `PublishEvent` emitted for any relay —
    /// structurally, an unroutable private recipient cannot reach the wire
    /// here because `relays` is never bound in that branch. Every borrow of
    /// `self.pending` below is scoped to its own statement so the map can
    /// be freely read/mutated/removed across steps.
    fn on_signed(&mut self, id: ReceiptId, event: SignedEvent, effects: &mut Vec<Effect>) {
        if let Some(pending) = self.pending.get_mut(&id) {
            pending.event_id = Some(event.id);
        } else {
            return; // unknown/already-resolved receipt id.
        }

        if let Some(pending) = self.pending.get(&id) {
            if let Some(sink) = &pending.sink {
                sink.on_status(WriteStatus::Signed(event.id));
            }
            if pending.sink.is_some() {
                effects.push(Effect::EmitReceipt(id, WriteStatus::Signed(event.id)));
            }
        }

        let author_hex = event.pubkey.to_hex();
        let route_result = match self.pending.get(&id) {
            Some(pending) => self.resolve_routes(&pending.routing, &author_hex),
            None => return,
        };

        let relays = match route_result {
            Ok(relays) => relays,
            Err(reason) => {
                if let Some(pending) = self.pending.remove(&id) {
                    let status = WriteStatus::Failed(reason);
                    if let Some(sink) = pending.sink {
                        sink.on_status(status.clone());
                        effects.push(Effect::EmitReceipt(id, status));
                    }
                }
                return;
            }
        };

        if let Some(pending) = self.pending.get(&id) {
            if let Some(sink) = &pending.sink {
                sink.on_status(WriteStatus::Routed(relays.clone()));
            }
            if pending.sink.is_some() {
                effects.push(Effect::EmitReceipt(id, WriteStatus::Routed(relays.clone())));
            }
        }

        for relay in &relays {
            effects.push(Effect::PublishEvent(relay.clone(), event.clone()));
        }
        if let Some(pending) = self.pending.get(&id) {
            if let Some(sink) = &pending.sink {
                for relay in &relays {
                    sink.on_status(WriteStatus::Sent(relay.clone()));
                }
            }
            if pending.sink.is_some() {
                for relay in &relays {
                    effects.push(Effect::EmitReceipt(id, WriteStatus::Sent(relay.clone())));
                }
            }
        }

        let ephemeral = matches!(
            self.pending.get(&id).map(|p| p.durability),
            Some(Durability::Ephemeral)
        );
        if ephemeral {
            // Fire-and-forget: no ack tracking, no receipt ever again.
            self.pending.remove(&id);
        } else if let Some(pending) = self.pending.get_mut(&id) {
            pending.pending_relays = relays;
            self.event_to_receipt.insert(event.id, id);
        }
    }

    /// Resolve a `WriteRouting` to a concrete relay set using the SAME
    /// `RelayDirectory` lane facts the read path routes against (plan
    /// §3.4). `AuthorOutbox` reuses the author's NIP-65 write-relay lane
    /// directly (the same fact `nmp_router::route::build_candidates` reads
    /// for outbox coverage-solving, minus the 2-relay-min solver — a write
    /// fans out to every known write relay, it does not need coverage-
    /// solving). `PrivateNarrow` never consults the directory at all — its
    /// relay set is exactly whatever the caller pre-narrowed into the
    /// `NarrowOnly` set, empty or not (ledger #6's fail-closed mechanism).
    ///
    /// DEVIATION (flagged, not silently papered over): `ToInboxes` wants
    /// each recipient's kind:10050/NIP-65 READ relays. `RelayDirectory`
    /// (nmp-router, out of this builder's scope) currently exposes only
    /// `write_relays`/`extra_relays`/`indexers`/`pinned_relays` — no
    /// per-pubkey read/inbox accessor exists. This falls back to the union
    /// of each recipient's `write_relays` + `extra_relays` as the closest
    /// available fact source; it is NOT the correct NIP-65 read-relay
    /// routing and should be replaced once `RelayDirectory` grows a
    /// dedicated accessor (a small additive change to `nmp-router`,
    /// coordinated separately per the scope boundary on this task).
    fn resolve_routes(
        &self,
        routing: &WriteRouting,
        author_hex: &str,
    ) -> Result<BTreeSet<RelayUrl>, String> {
        match routing {
            WriteRouting::AuthorOutbox => {
                let author = author_hex.to_string();
                let relays: BTreeSet<RelayUrl> = self
                    .directory
                    .write_relays(&author)
                    .into_iter()
                    .map(|lr| lr.url)
                    .collect();
                if relays.is_empty() {
                    Err(format!("no write relays known for author {author_hex}"))
                } else {
                    Ok(relays)
                }
            }
            WriteRouting::ToInboxes(recipients) => {
                let mut relays = BTreeSet::new();
                for pk in recipients {
                    let hex = pk.to_hex();
                    relays.extend(
                        self.directory
                            .write_relays(&hex)
                            .into_iter()
                            .map(|lr| lr.url),
                    );
                    relays.extend(
                        self.directory
                            .extra_relays(&hex)
                            .into_iter()
                            .map(|lr| lr.url),
                    );
                }
                if relays.is_empty() {
                    Err("no recipient inbox relays resolved".to_string())
                } else {
                    Ok(relays)
                }
            }
            WriteRouting::PrivateNarrow(route) => {
                if route.relays.is_empty() {
                    Err(
                        "private route has no narrow relay set -- fails closed, never widens to a public relay"
                            .to_string(),
                    )
                } else {
                    Ok(route.relays.iter().cloned().collect())
                }
            }
        }
    }

    /// An `OK` frame resolves exactly one (event, relay) pair's pending
    /// ack. An `OK` for an event/relay this reducer isn't tracking (unknown
    /// event id, already-terminal receipt, duplicate OK, or an `Ephemeral`
    /// write that was already forgotten) is silently ignored — it is an
    /// untrusted-network fact, not a caller error.
    fn handle_write_ack(
        &mut self,
        event_id: EventId,
        status: bool,
        message: String,
        relay: &RelayUrl,
        effects: &mut Vec<Effect>,
    ) {
        let Some(&id) = self.event_to_receipt.get(&event_id) else {
            return;
        };
        let Some(pending) = self.pending.get_mut(&id) else {
            return;
        };
        if !pending.pending_relays.remove(relay) {
            return;
        }
        let new_status = if status {
            WriteStatus::Acked(relay.clone())
        } else {
            WriteStatus::Rejected(relay.clone(), message)
        };
        if let Some(sink) = &pending.sink {
            sink.on_status(new_status.clone());
        }
        effects.push(Effect::EmitReceipt(id, new_status));
        if pending.pending_relays.is_empty() {
            self.pending.remove(&id);
            self.event_to_receipt.remove(&event_id);
        }
    }

    /// A relay disconnecting before it ever acked a pending write is a
    /// terminal `GaveUp` for every write still waiting on it — NOT a retry.
    /// No durability class re-sends here (ledger's `AtMostOnce` amendment:
    /// "no blind retry"; `Durable`'s stronger ack tracking is about
    /// accuracy of the receipt stream, not automatic resend, which this
    /// builder does not implement — see the report).
    fn give_up_pending_writes(&mut self, relay: &RelayUrl, effects: &mut Vec<Effect>) {
        let ids: Vec<ReceiptId> = self
            .pending
            .iter()
            .filter(|(_, p)| p.pending_relays.contains(relay))
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let event_id = if let Some(pending) = self.pending.get_mut(&id) {
                pending.pending_relays.remove(relay);
                let status = WriteStatus::GaveUp(relay.clone());
                if let Some(sink) = &pending.sink {
                    sink.on_status(status.clone());
                }
                effects.push(Effect::EmitReceipt(id, status));
                if pending.pending_relays.is_empty() {
                    pending.event_id
                } else {
                    None
                }
            } else {
                None
            };
            if event_id.is_some() {
                self.pending.remove(&id);
            }
            if let Some(event_id) = event_id {
                self.event_to_receipt.remove(&event_id);
            }
        }
    }

    fn alloc_receipt_id(&mut self) -> ReceiptId {
        self.next_receipt += 1;
        ReceiptId(self.next_receipt)
    }

    // ---- transport wiring (slot bookkeeping only — C owns the pool) -----

    fn on_relay_connected(&mut self, handle: TransportRelayHandle, url: RelayUrl) -> Vec<Effect> {
        self.slot_to_url.insert(handle.slot, url.clone());
        // Reconnect (new generation): clear stale attribution, then replay
        // + re-snapshot every currently-planned REQ for this relay (ruling
        // §2: "a replayed sub on the new generation gets fresh snapshots").
        self.attribution.clear_relay(&url);
        let mut effects = Vec::new();
        if let Some(reqs) = self.router.plan().reqs.get(&url).cloned() {
            if !reqs.is_empty() {
                for req in &reqs {
                    self.attribution.record_send(
                        &url,
                        &req.sub_id,
                        &req.filter,
                        req.absorbed.clone(),
                    );
                }
                effects.push(Effect::Replay(url, reqs));
            }
        }
        effects
    }

    fn on_relay_disconnected(&mut self, slot: u32) -> Vec<Effect> {
        let mut effects = Vec::new();
        if let Some(url) = self.slot_to_url.get(&slot).cloned() {
            self.attribution.clear_relay(&url);
            self.give_up_pending_writes(&url, &mut effects);
        }
        effects
    }

    // ---- inbound relay frame: EVENT/EOSE parsed here (D/E own OK/CLOSED/
    // NOTICE/AUTH/COUNT/NEG-*) --------------------------------------------

    fn on_relay_frame(&mut self, handle: TransportRelayHandle, frame: RelayFrame) -> Vec<Effect> {
        let mut effects = Vec::new();
        let RelayFrame::Text(text) = frame else {
            // AUTH/NIP-42 handshake is deferred (plan §7 non-goal unless a
            // falsifier test forces it) — not B's job.
            return effects;
        };
        let Ok(msg) = RelayMessage::from_json(text.as_bytes()) else {
            return effects; // malformed frame: an untrusted-network fact, not a panic.
        };
        let Some(relay) = self.slot_to_url.get(&handle.slot).cloned() else {
            return effects; // frame from a slot we never saw RelayConnected for.
        };

        match msg {
            RelayMessage::Event { event, .. } => {
                let observed = RelayObserved::new(relay, self.clock);
                let _delta = self
                    .resolver
                    .ingest_observed(vec![(event.into_owned(), observed)]);
                self.recompile(&mut effects);
                self.refresh_all_handles(&mut effects);
            }
            RelayMessage::EndOfStoredEvents(sub_id) => {
                let attributed =
                    self.attribution
                        .attribute_eose(&relay, sub_id.as_str(), self.clock);
                for (key, interval) in attributed {
                    if let Some(shape) = self.attribution.shape_of(key) {
                        self.resolver
                            .store_mut()
                            .record_coverage(&shape, &relay, interval);
                        effects.push(Effect::RecordCoverage(key, relay.clone(), interval));
                    }
                }
                // A watermark advancing can flip a handle's QueryCoverage
                // (ruling §6) even with no new rows at all — refresh so
                // that becomes observable via EmitRows, same as an ingest.
                self.refresh_all_handles(&mut effects);
            }
            RelayMessage::Ok {
                event_id,
                status,
                message,
            } => {
                self.handle_write_ack(event_id, status, message.into_owned(), &relay, &mut effects);
            }
            // Closed/Notice/Auth/Count/NegMsg/NegErr: E (negentropy) and
            // AUTH-handshake territory, not built in D.
            _ => {}
        }
        effects
    }

    // ---- shared recompile + row-refresh plumbing -------------------------

    /// Recompile the router from the resolver's CURRENT demand, record any
    /// newly-sent REQs' attribution snapshots, and push `Effect::Wire` if
    /// anything actually changed on the wire.
    fn recompile(&mut self, effects: &mut Vec<Effect>) {
        let demand = self.resolver.active_demand();
        self.attribution.observe_demand(demand.iter());
        let wire_delta: WireDelta = self
            .router
            .compile(&demand, self.directory.as_ref(), self.cap);
        if wire_delta.ops.is_empty() {
            return;
        }
        for (relay, ops) in &wire_delta.ops {
            for op in ops {
                if let WireOp::Req(sub_id, filter) = op {
                    let absorbed = self
                        .router
                        .plan()
                        .reqs
                        .get(relay)
                        .and_then(|reqs| reqs.iter().find(|r| &r.sub_id == sub_id))
                        .map(|r| r.absorbed.clone())
                        .unwrap_or_default();
                    self.attribution
                        .record_send(relay, sub_id, filter, absorbed);
                }
            }
        }
        effects.push(Effect::Wire(wire_delta));
    }

    fn refresh_all_handles(&mut self, effects: &mut Vec<Effect>) {
        let ids: Vec<HandleId> = self.handles.keys().copied().collect();
        for id in ids {
            self.refresh_handle(id, effects);
        }
    }

    /// Recompute `id`'s current row set + query coverage; emit (and
    /// synchronously deliver to its sink) `Effect::EmitRows` only if either
    /// changed since the last refresh.
    fn refresh_handle(&mut self, id: HandleId, effects: &mut Vec<Effect>) {
        let (rows, coverage) = self.rows_and_coverage_for(id);
        let Some(state) = self.handles.get_mut(&id) else {
            return;
        };
        let row_ids: BTreeSet<EventId> = rows.iter().map(|r| r.event.id).collect();
        if row_ids == state.last_rows && state.last_coverage == Some(coverage) {
            return;
        }
        state.last_rows = row_ids;
        state.last_coverage = Some(coverage);
        state.sink.on_rows(rows.clone());
        effects.push(Effect::EmitRows(id, rows, coverage));
    }

    fn rows_and_coverage_for(&self, id: HandleId) -> (Vec<RowDelta>, QueryCoverage) {
        let atoms = self.resolver.root_atoms(id);
        let mut by_id: BTreeMap<EventId, nostr::Event> = BTreeMap::new();
        for atom in &atoms {
            for se in self.resolver.store().query(&atom.to_nostr()) {
                by_id.entry(se.event.id).or_insert(se.event);
            }
        }
        let rows = by_id
            .into_values()
            .map(|event| RowDelta { event })
            .collect();
        let coverage =
            coverage_query::query_coverage(&atoms, self.router.plan(), self.resolver.store());
        (rows, coverage)
    }
}
