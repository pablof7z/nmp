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
use crate::outbox::{ReceiptSink, WriteIntent, WriteStatus};

use attribution::AttributionState;
pub use coverage_query::QueryCoverage;

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
            EngineMsg::SignerCompleted(_id, _result) => {
                // Sign-orchestration is D territory (depends on B + A3); B
                // only needs an exhaustive, harmless match arm.
                Vec::new()
            }
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

    // ---- write outbox (B stub — D owns the real orchestration) ----------

    fn on_publish(&mut self, _intent: WriteIntent, sink: Box<dyn ReceiptSink>) -> Vec<Effect> {
        let id = self.alloc_receipt_id();
        sink.on_status(WriteStatus::Accepted);
        vec![Effect::EmitReceipt(id, WriteStatus::Accepted)]
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
        if let Some(url) = self.slot_to_url.get(&slot).cloned() {
            self.attribution.clear_relay(&url);
        }
        Vec::new()
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
            // OK/Closed/Notice/Auth/Count/NegMsg/NegErr: D (write acks) and
            // E (negentropy) territory, not built in B.
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
