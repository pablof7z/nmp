//! The PURE synchronous reducer (plan §2 position 1, §3.4). `EngineCore`
//! owns the M1 resolver `Engine<S>`, the M2 `Router`, the write-outbox
//! state, and the coverage-attribution bookkeeping (`attribution.rs`,
//! `evidence.rs`). Its entire surface is:
//!
//! ```ignore
//! impl<S: EventStore> EngineCore<S> {
//!     pub fn handle(&mut self, msg: EngineMsg) -> Vec<Effect>;
//!     pub fn tick(&mut self, now: nostr::Timestamp) -> Vec<Effect>;
//!     pub fn next_deadline(&self) -> Option<nostr::Timestamp>;
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
//! [`attribution`]; the per-query, per-source acquisition evidence (`rows +
//! compact facts, never a collapsed global verdict` —
//! `docs/design/scoped-evidence-49-12-plan.md`, folding #12 into #49) lives
//! in [`evidence`]. Both are engine-owned — the store (`nmp-store`) only
//! stores whatever interval it is handed.

mod attribution;
mod diagnostics;
mod evidence;

use std::collections::{BTreeMap, BTreeSet, HashMap};

use nostr::{
    Event as SignedEvent, EventId, JsonUtil, PublicKey, RelayMessage, RelayUrl, Timestamp,
    UnsignedEvent,
};

use nmp_grammar::{Binding, ConcreteFilter, Filter};
use nmp_resolver::{Engine as ResolverEngine, HandleId, LiveQuery, QueryHandle};
use nmp_router::{
    DiscoveryKinds, Lane, LanedRelay, PubkeyHex, RelayDirectory, RelayLimits, Router, RuleRegistry,
    SubId, WireDelta, WireOp, WireReq,
};
use nmp_signer::SignerError;
use nmp_store::{CoverageKey, EventStore, RelayObserved};
use nmp_transport::{RelayFrame, RelayHandle as TransportRelayHandle};

use crate::negentropy::{NegStep, ProbedRelay, Prober, Reconciler};
use crate::outbox::{
    Durability, ReceiptSink, WriteIntent, WritePayload, WriteRouting, WriteStatus,
};

/// The liveness deadline (plan §4/harvest `nmp-nip77`) past which an open
/// negentropy session with no reply is abandoned in favor of a plain REQ
/// (never left to hang forever, and never silently re-tried as negentropy
/// again on the same generation -- `tick`'s own staleness sweep is the only
/// caller of this constant).
const NEG_LIVENESS_DEADLINE_SECS: u64 = 30;

/// NIP-65 Relay List Metadata — the kind the self-bootstrapping outbox (M5)
/// auto-discovers for any author the current demand references but whose
/// write relays the directory doesn't know yet (see [`EngineCore::
/// sync_discovery`]). Already a member of `nmp_router::DiscoveryKinds`'s
/// default set, so the router routes this atom to the configured indexers
/// with NO router-side changes of its own -- the same `build_candidates`
/// eligibility check that already applies to kind:3/kind:0/kind:10050.
const NIP65_RELAY_LIST_KIND: u16 = 10_002;

use attribution::AttributionState;
pub use diagnostics::{DiagnosticsSnapshot, FilterCoverageEntry, RelayDiagnosticsSnapshot};
pub use evidence::{AcquisitionEvidence, AuthPhase, ShortfallFact, SourceEvidence, SourceStatus};
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

/// A row-set delta (plan §7 non-goal: no ordering/windowing in M3 — raw
/// deltas + coverage only). This is the standard reactive-query contract:
/// `Effect::EmitRows`/`RowSink::on_rows` NEVER re-sends the query's full
/// current row set -- only the rows ADDED and REMOVED since that handle's
/// LAST emit (`refresh_handle`'s job). The FIRST emit for a fresh subscribe
/// is "every currently-matching row, as `Added`" (there is nothing to diff
/// against yet); an identity re-root (`set_active_pubkey`) that swaps the
/// whole row set falls out of the SAME diff -- "remove everything old, add
/// everything new" -- with no special-casing. Without this contract, a
/// long-running subscription that keeps matching new events re-delivers its
/// ENTIRE growing row set on every single ingest: O(rows) work per event,
/// O(rows²) total over a session (confirmed live: ~3.35M raw row deliveries
/// for ~2,587 distinct notes in 20s against real relays --
/// `docs/known-gaps.md`'s P0).
#[derive(Debug, Clone)]
pub enum RowDelta {
    /// A row that newly matches the query, carrying the full event so the
    /// app never has to look it up separately.
    Added(nostr::Event),
    /// A row that no longer matches the query. Carries only the id -- the
    /// app is expected to already hold the event from an earlier `Added`
    /// (raw deltas + coverage only: no second copy of the payload is kept
    /// around just to hand back on removal).
    Removed(EventId),
}

impl RowDelta {
    /// The event id this delta concerns, regardless of variant.
    pub fn id(&self) -> EventId {
        match self {
            RowDelta::Added(event) => event.id,
            RowDelta::Removed(id) => *id,
        }
    }

    /// The event payload, if this is an `Added` delta (`None` for
    /// `Removed`).
    pub fn event(&self) -> Option<&nostr::Event> {
        match self {
            RowDelta::Added(event) => Some(event),
            RowDelta::Removed(_) => None,
        }
    }
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
/// carries the query's [`AcquisitionEvidence`] alongside its rows
/// (`docs/design/scoped-evidence-49-12-plan.md`): per-source acquisition
/// facts over the query's FULL subtree (interior `Derived` atoms included,
/// #12), never a single collapsed query-global verdict — an app reads
/// which source has proven what, it is never handed a settled/complete
/// judgment.
#[derive(Debug)]
pub enum Effect {
    /// -> `Pool::send` per (relay, current handle).
    Wire(WireDelta),
    /// Reconnect: resend the current wire subs on the NEW generation.
    Replay(RelayUrl, Vec<WireReq>),
    /// Place a capability-probing `NEG-OPEN` on the wire (`negentropy::
    /// Prober::begin_probe`'s output, carried in full since the runtime has
    /// no negentropy-protocol knowledge of its own): the sub-id, the
    /// throwaway probe filter, and the hex initial message.
    StartProbe(RelayUrl, SubId, ConcreteFilter, String),
    /// Place a REAL negentropy-first `NEG-OPEN` for `filter` against a
    /// PROVEN-supported relay (ledger #8's compile-fence: the first field
    /// can only ever be a `ProbedRelay`), under `sub_id`, with the hex
    /// initial message this reducer already built from its own store.
    NegOpen(ProbedRelay, SubId, ConcreteFilter, String),
    /// Continue an open reconciliation: place this hex payload as the next
    /// outbound `NEG-MSG` for `sub_id` on `relay`.
    NegMsg(RelayUrl, SubId, String),
    /// Release `sub_id` on `relay` (`NEG-CLOSE`) -- reconciliation finished,
    /// was abandoned (liveness deadline / `NEG-ERR`), or is being converted
    /// back to a plain REQ.
    NegClose(RelayUrl, SubId),
    /// One per attributed atom per EOSE/NEG-DONE (ruling §7): the narrow
    /// atom's `CoverageKey`, the relay that proved it, and the proven
    /// interval.
    RecordCoverage(
        nmp_store::CoverageKey,
        RelayUrl,
        nmp_store::CoverageInterval,
    ),
    EmitRows(HandleId, Vec<RowDelta>, AcquisitionEvidence),
    /// The engine-global diagnostics projection (M5 plan §1.2 step 3),
    /// pushed at the end of every `recompile()` and after every EOSE
    /// (coverage watermarks can advance with no recompile at all). Read-only
    /// and off the data path -- never influences routing/delivery.
    /// `runtime::Handle::observe_diagnostics` forwards this to every
    /// registered observer, latest-wins if a consumer is slow (never
    /// buffered/replayed).
    EmitDiagnostics(DiagnosticsSnapshot),
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
/// the last-emitted row-id/evidence pair (so `EmitRows` fires only when
/// something actually changed, not on every unrelated recompile).
/// `AcquisitionEvidence` derives `PartialEq` precisely so this
/// change-detection compare stays a plain value comparison, as the former
/// query-evidence aggregate's did.
struct HandleState {
    _handle: QueryHandle,
    sink: Box<dyn RowSink>,
    last_rows: BTreeSet<EventId>,
    last_evidence: Option<AcquisitionEvidence>,
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

/// A live, EngineCore-owned negentropy reconciliation in progress for
/// `sub_id` (plan §6 E). `filter` is already window-erased (since/until/
/// limit cleared) -- ruling §2: "NEG runs unfloored/unlimited"; recording an
/// attribution snapshot straight off this field is therefore always the
/// correct floor:None/until:None/limited:false snapshot the ruling
/// requires, with no separate bookkeeping to keep in sync.
struct NegSession {
    relay: RelayUrl,
    filter: ConcreteFilter,
    absorbed: BTreeSet<CoverageKey>,
    started_at: Timestamp,
    reconciler: Reconciler,
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
    /// Relays CURRENTLY connected — feeds `AcquisitionEvidence.sources[_]
    /// .status` (`Requesting` iff a member here covers the atom;
    /// `Disconnected` iff it was a member of `ever_connected_relays` but
    /// isn't a member here; `Connecting` otherwise). Additive bookkeeping:
    /// `slot_to_url`'s own semantics (populated on connect, never cleared on
    /// disconnect) are untouched by this.
    connected_relays: BTreeSet<RelayUrl>,
    /// Every relay that has connected at least once, ever — distinguishes
    /// `Disconnected` (was connected, dropped) from `Connecting` (never yet
    /// connected) for the same evidence computation.
    ever_connected_relays: BTreeSet<RelayUrl>,
    clock: Timestamp,
    next_receipt: u64,
    /// Write outbox (§3.4 / VISION §7 ledger #6/#9). `pending` is keyed by
    /// `ReceiptId` from `Publish` through to the last terminal per-relay
    /// status; `event_to_receipt` lets an inbound `OK` frame (keyed by
    /// `EventId` on the wire) find its receipt.
    pending: HashMap<ReceiptId, PendingWrite>,
    event_to_receipt: HashMap<EventId, ReceiptId>,
    /// The negentropy capability-probe cache (plan §6 E).
    prober: Prober,
    /// Live reconciliation sessions, keyed by the SAME `SubId` a plain REQ
    /// for this shape would have used (REQ and negentropy share one
    /// subscription-id namespace on the wire, NIP-77) -- never more than one
    /// entry per sub-id at a time.
    neg_sessions: HashMap<SubId, NegSession>,
    /// One-shot `ids`-filter REQs opened to backfill exactly what a
    /// completed reconciliation proved we are missing (`finish_neg_session`)
    /// -- tracked so this reducer closes them itself once their EOSE
    /// arrives, rather than leaking a subscription the router's own
    /// demand-diffing does not know about.
    pending_backfills: BTreeSet<SubId>,
    /// Backfill `SubId` -> the reconciled negentropy session's own `SubId`,
    /// whose coverage credit is deferred until THIS backfill's EOSE proves
    /// the missing events actually landed (ledger #7 -- see
    /// `finish_neg_session`'s doc comment).
    pending_neg_credit: HashMap<SubId, SubId>,
    /// The self-bootstrapping outbox (M5): an internal, engine-owned
    /// resolver subscription discovering kind:10002 for exactly the authors
    /// current demand references but whose write relays are still unknown
    /// (see [`Self::sync_discovery`]). `None` when no author currently needs
    /// discovering. The app never sees this handle or this atom -- it rides
    /// the SAME demand/atom/router machinery every other subscription does,
    /// never a parallel subscription system.
    discovery_handle: Option<QueryHandle>,
    /// The exact author set `discovery_handle` (if any) is currently open
    /// for -- compared against the freshly-computed "needed" set on every
    /// `sync_discovery` call so the subscription is only replaced when the
    /// set actually changes, not on every recompile.
    discovery_authors: BTreeSet<PubkeyHex>,
    /// The diagnostic surface's own counter (M5 plan §1.2 step 1) — events
    /// actually RECEIVED, per relay per kind. Bumped in the
    /// `RelayMessage::Event` arm of `on_relay_frame`; read (never mutated)
    /// by `diagnostics_snapshot`. This is the one datum `nmp-router`'s
    /// `Diagnostics` cannot see on its own — it never observes inbound
    /// frames, only what was compiled/sent.
    events_by_relay_kind: HashMap<RelayUrl, BTreeMap<u16, u64>>,
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
            connected_relays: BTreeSet::new(),
            ever_connected_relays: BTreeSet::new(),
            clock: Timestamp::from(0u64),
            next_receipt: 0,
            pending: HashMap::new(),
            event_to_receipt: HashMap::new(),
            prober: Prober::new(),
            neg_sessions: HashMap::new(),
            pending_backfills: BTreeSet::new(),
            pending_neg_credit: HashMap::new(),
            discovery_handle: None,
            discovery_authors: BTreeSet::new(),
            events_by_relay_kind: HashMap::new(),
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

    /// The engine-global diagnostics projection (M5 plan §1.2 step 2) — "the
    /// acceptance test made visible": combines `nmp_router::Router::
    /// diagnostics()` (per-relay wire-sub count, exact filters, lane
    /// counts, reverse coverage) with this reducer's own `events_by_relay_
    /// kind` counter and per-(relay, filter) coverage read via
    /// `Self::get_coverage`. Pure and read-only — never influences
    /// routing/delivery; every number here is real state this reducer
    /// already tracks for other reasons, never fabricated/estimated.
    pub fn diagnostics_snapshot(&self) -> DiagnosticsSnapshot {
        diagnostics::build(
            self.router.diagnostics(),
            self.router.plan(),
            &self.events_by_relay_kind,
            |relay, key| self.resolver.store().get_coverage(key, relay),
        )
    }

    /// A pure clock update PLUS two deadline sweeps: NIP-40 expiry
    /// (retraction-and-negative-deltas.md §3.2 — drains `store.expire_due`
    /// and retracts every row past its deadline) and the negentropy
    /// liveness-deadline sweep (plan §6 E, harvest `nmp-nip77`'s "30s
    /// liveness-deadline REQ fallback"): any reconciliation session open
    /// longer than [`NEG_LIVENESS_DEADLINE_SECS`] against `now` is
    /// abandoned in favor of a plain REQ for the same (unfloored/unlimited)
    /// filter. Backoff/keepalive scheduling stays D/A2 territory --
    /// untouched here.
    ///
    /// `runtime::engine_loop` (§3.3, #39) is what actually drives this on
    /// its own now: it arms `cmd_rx.recv_timeout` off [`Self::next_deadline`]
    /// and dispatches `EngineMsg::Tick(wall_now())` exactly when that
    /// timeout elapses (D8: the existing blocking recv grows a timeout,
    /// never a poll-loop timer thread). Both sweeps stay real and unit-
    /// tested here against a synthetic clock regardless of who calls this
    /// -- the runtime driver is a caller, not part of the mechanism.
    pub fn tick(&mut self, now: Timestamp) -> Vec<Effect> {
        self.clock = now;
        let mut effects = Vec::new();

        // NIP-40 expiry (retraction-and-negative-deltas.md §3.2) — manual
        // for now, same caveat as the neg-liveness sweep below: nothing yet
        // drives `Tick` on its own cadence (the `recv_timeout`
        // deadline-armed runtime driver is a separate #23 child, §3.3).
        // Drain every row whose expiration is due straight through the
        // store's own index (`O(log n + due)`, never a scan), then route
        // the removed rows through the SAME retraction lane a kind:5
        // delete already uses inside `ingest_observed` — `resolver.retract`
        // seeds dirty-marks from `removed` alone, `recompile` + `refresh_
        // all_handles` do the rest, and `RowDelta::Removed` falls out the
        // ordinary way.
        let expired = self.resolver.store_mut().expire_due(now);
        if !expired.is_empty() {
            let removed: Vec<_> = expired.into_iter().map(|se| se.event).collect();
            let _delta = self.resolver.retract(removed);
            self.recompile(&mut effects);
            self.refresh_all_handles(&mut effects);
        }

        // `>=` against the EXACT `Timestamp` threshold `next_deadline()`
        // arms for (`started_at + NEG_LIVENESS_DEADLINE_SECS`) -- not the
        // `as_secs()`-truncated, strictly-greater subtraction this used to
        // be. Those two must reference the identical expression: the
        // runtime driver's `recv_timeout` wakes AT the deadline it was
        // armed for (`duration_until` floors an already-reached deadline to
        // zero), so a strict `>` here left the sweep still false at that
        // exact `now`, `next_deadline()` still returning the same
        // deadline, and `duration_until` still flooring to zero -- a
        // `recv_timeout(0)` busy-spin until the wall clock ticked over into
        // the NEXT whole second (`as_secs()` finally reading `31 > 30`).
        // `>=` clears the session in the very tick that reaches its
        // deadline, so `next_deadline()` recomputes without it and the loop
        // parks -- see #39's fix-up review and the regression test this
        // predicate exists to satisfy.
        let stale: Vec<SubId> = self
            .neg_sessions
            .iter()
            .filter(|(_, s)| now >= s.started_at + NEG_LIVENESS_DEADLINE_SECS)
            .map(|(id, _)| id.clone())
            .collect();
        for sub_id in stale {
            if let Some(session) = self.neg_sessions.remove(&sub_id) {
                self.neg_session_fallback_to_req(sub_id, session, &mut effects);
            }
        }

        effects
    }

    /// The earliest wall-clock instant at which [`Self::tick`] must run for
    /// something to actually happen (retraction-and-negative-deltas.md
    /// §3.2): the min over every deadline source this reducer currently
    /// tracks -- NIP-40 expiry (`store.next_expiration()`, index-backed) and
    /// open negentropy sessions' liveness deadlines (`started_at +
    /// NEG_LIVENESS_DEADLINE_SECS`). `None` means no timer needs to fire at
    /// all right now: `runtime::engine_loop`'s `recv_timeout` driver (§3.3)
    /// sleeps forever on the plain `recv()` in that case, exactly matching
    /// the doc's "a light embedder with no deadlines pays nothing".
    /// Extensible to future timers (backoff, drop-grace debounce) by folding
    /// another `.min()` term in here -- the runtime driver itself never
    /// needs to change to pick up a new deadline source.
    pub fn next_deadline(&self) -> Option<Timestamp> {
        let expiry = self.resolver.store().next_expiration();
        let neg_liveness = self
            .neg_sessions
            .values()
            .map(|session| session.started_at + NEG_LIVENESS_DEADLINE_SECS)
            .min();
        match (expiry, neg_liveness) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        }
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
        // A new query can change the capped greedy source plan for EVERY
        // existing query, even when their rows are unchanged. Refresh the
        // survivors against the newly-finalized plan before installing the
        // new handle; otherwise their "current-plan" evidence can retain a
        // source that the router just dropped (or omit one it just added).
        self.refresh_all_handles(&mut effects);
        self.handles.insert(
            id,
            HandleState {
                _handle: qh,
                sink,
                last_rows: BTreeSet::new(),
                last_evidence: None,
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
        // Removing one query can free capped-plan capacity and therefore
        // change the planned sources of every surviving handle.
        self.refresh_all_handles(&mut effects);
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
    ///
    /// A `Signed` payload is verified here, at the acceptance boundary,
    /// BEFORE `WriteStatus::Accepted` is ever emitted (#52 Q2). This is the
    /// only publish path in the crate — `Handle::publish` is the sole entry
    /// point regardless of caller (FFI, direct-Rust, `nmp-bdd`'s
    /// `EngineThread`) — so verifying here, rather than at each caller,
    /// makes "a forged `Signed` event can never be published" true
    /// unconditionally instead of entry-point-dependent. A failed verify is
    /// a whole-intent terminal (`WriteStatus::Failed`): no `Accepted`, no
    /// pending write recorded, no `Effect::PublishEvent`.
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

        if let WritePayload::Signed(event) = &payload {
            if let Err(err) = event.verify() {
                let status = WriteStatus::Failed(err.to_string());
                return match sink {
                    Some(sink) => {
                        sink.on_status(status.clone());
                        vec![Effect::EmitReceipt(id, status)]
                    }
                    None => Vec::new(),
                };
            }
        }

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
    /// `ToInboxes` fans a p-tagged inbox write out to each recipient's
    /// NIP-65 READ-marked relays (`RelayDirectory::read_relays`, lane
    /// `Nip65Read`) — the read side of the SAME kind:10002 winner the read
    /// path consults for authors' write relays (`routing-and-ownership.md`
    /// §2.4). It NEVER consults a recipient's `write_relays`/`extra_relays`:
    /// addressing inbox traffic to a recipient's write relays under-delivers
    /// and leaks metadata (issue #19). A recipient whose read/inbox relays
    /// are unknown — never seen a kind:10002, or one that declares only
    /// write-marked relays — fails the whole intent CLOSED with a typed
    /// `Failed` before any `PublishEvent`, rather than guessing a relay;
    /// recipient discovery rides the existing kind:10002 `sync_discovery`
    /// machinery, so a later winner simply makes the retry routable.
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
                    // Read/inbox relays ONLY (lane `Nip65Read`) — never a
                    // recipient's write/extra relays. Fail CLOSED per
                    // recipient: an unknown or write-only recipient has no
                    // inbox relay, and guessing one would leak/under-deliver.
                    let inbox: Vec<RelayUrl> = self
                        .directory
                        .read_relays(&hex)
                        .into_iter()
                        .map(|lr| lr.url)
                        .collect();
                    if inbox.is_empty() {
                        return Err(format!(
                            "no NIP-65 read/inbox relays known for recipient {hex} -- \
                             inbox route fails closed, never falls back to write relays"
                        ));
                    }
                    relays.extend(inbox);
                }
                if relays.is_empty() {
                    Err("ToInboxes routing has no recipients".to_string())
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
        // Feeds `AcquisitionEvidence.sources[_].status` (`evidence.rs`):
        // this relay is now `Requesting`, never again `Connecting` for the
        // lifetime of this `EngineCore` (`ever_connected_relays` is
        // append-only -- a later drop reads `Disconnected`, not
        // `Connecting`, per the doc's "was connected, then dropped" fact).
        self.connected_relays.insert(url.clone());
        self.ever_connected_relays.insert(url.clone());
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
                effects.push(Effect::Replay(url.clone(), reqs));
            }
        }
        // Capability probe (plan §6 E): idempotent -- a relay whose verdict
        // is already cached (`Supported`/`Unsupported`) from an earlier
        // connection on this same `Prober` is never re-probed.
        if let Some(probe) = self.prober.begin_probe(&url) {
            effects.push(Effect::StartProbe(
                url,
                probe.sub_id,
                probe.filter,
                probe.initial_message_hex,
            ));
        }
        // A relay coming online can flip a handle's `AcquisitionEvidence`
        // (`Connecting` -> `Requesting`) with no coverage/row change at all
        // -- refresh so that becomes observable via `EmitRows`, same as an
        // EOSE-driven watermark advance below.
        self.refresh_all_handles(&mut effects);
        effects
    }

    fn on_relay_disconnected(&mut self, slot: u32) -> Vec<Effect> {
        let mut effects = Vec::new();
        if let Some(url) = self.slot_to_url.get(&slot).cloned() {
            self.attribution.clear_relay(&url);
            self.give_up_pending_writes(&url, &mut effects);
            // Any reconciliation open against this relay dies with the
            // connection -- there is nothing left to `NEG-CLOSE` (the socket
            // is already gone), so this is a silent drop, not a fallback
            // REQ: the relay's own `Supported` verdict stays cached, and the
            // NEXT `recompile()`/reconnect naturally re-opens whatever
            // demand still wants this shape.
            self.neg_sessions.retain(|_, session| session.relay != url);
            // Feeds `AcquisitionEvidence.sources[_].status`: this relay is
            // no longer connected, but `ever_connected_relays` is untouched
            // -- a subsequent evidence computation reads `Disconnected`,
            // never `Connecting`, and any `reconciled_through` this relay
            // already earned survives (the #49 "offline cached rows remain
            // usable" acceptance criterion -- watermark and link status are
            // deliberately orthogonal fields, never one enum).
            self.connected_relays.remove(&url);
        }
        // Same reasoning as `on_relay_connected`: a link-status flip alone
        // must become observable via `EmitRows`.
        self.refresh_all_handles(&mut effects);
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
                let event = event.into_owned();
                // The diagnostic surface's own counter (M5 plan §1.2 step
                // 1) — genuinely not tracked anywhere else: bump BEFORE
                // `recompile()` below so the `EmitDiagnostics` it pushes
                // already reflects this event.
                *self
                    .events_by_relay_kind
                    .entry(relay.clone())
                    .or_default()
                    .entry(event.kind.as_u16())
                    .or_insert(0) += 1;
                // M5 self-bootstrapping outbox: a kind:10002 needs its
                // author's write relays fed into the live directory BEFORE
                // `recompile()` runs, so the very same recompile that
                // ingested it can already route that author's content atoms
                // to the newly-known relay (see `ingest_relay_list_winner`'s
                // doc for why this re-reads the store's winner rather than
                // trusting this frame directly).
                let relay_list_author =
                    (event.kind == nostr::Kind::RelayList).then_some(event.pubkey);
                let observed = RelayObserved::new(relay, self.clock);
                let _delta = self.resolver.ingest_observed(vec![(event, observed)]);
                if let Some(author) = relay_list_author {
                    self.ingest_relay_list_winner(author);
                }
                self.recompile(&mut effects);
                self.refresh_all_handles(&mut effects);
            }
            RelayMessage::EndOfStoredEvents(sub_id) => {
                let wire_id = sub_id.as_str();
                let attributed = self.attribution.attribute_eose(&relay, wire_id, self.clock);
                for (key, interval) in attributed {
                    if let Some(shape) = self.attribution.shape_of(key) {
                        self.resolver
                            .store_mut()
                            .record_coverage(&shape, &relay, interval);
                        effects.push(Effect::RecordCoverage(key, relay.clone(), interval));
                    }
                }
                // A watermark advancing can flip a handle's
                // AcquisitionEvidence (a source's `reconciled_through`) even
                // with no new rows at all — refresh so that becomes
                // observable via EmitRows, same as an ingest.
                self.refresh_all_handles(&mut effects);
                // Same watermark advance can also flip the diagnostic
                // surface's own per-(filter, relay) coverage even though
                // this arm never calls `recompile()` (M5 plan §1.2 step 3:
                // "after the Event/EOSE ingest arms ... coverage change
                // points").
                effects.push(Effect::EmitDiagnostics(self.diagnostics_snapshot()));

                // A one-shot negentropy backfill REQ (`finish_neg_session`)
                // has nothing further to prove once it EOSEs -- close it so
                // it does not linger as a subscription the router's own
                // demand-diffing never knew existed, and -- if it was
                // deferring a reconciliation's coverage credit -- THIS is
                // the moment the backfilled events are proven ingested
                // (EVENT precedes EOSE, NIP-01), so it is now safe to credit
                // (ledger #7: never before this point).
                if let Some(resolved) = self.attribution.sub_id_for_wire(&relay, wire_id) {
                    if self.pending_backfills.remove(&resolved) {
                        effects.push(Effect::Wire(WireDelta {
                            ops: vec![(relay.clone(), vec![WireOp::Close(resolved.clone())])],
                        }));
                    }
                    if let Some(original_sub_id) = self.pending_neg_credit.remove(&resolved) {
                        self.credit_neg_coverage(&original_sub_id, &relay, &mut effects);
                    }
                }
            }
            RelayMessage::Ok {
                event_id,
                status,
                message,
            } => {
                self.handle_write_ack(event_id, status, message.into_owned(), &relay, &mut effects);
            }
            RelayMessage::NegMsg {
                subscription_id,
                message,
            } => {
                let wire_id = subscription_id.as_str();
                if self.prober.on_neg_msg(&relay, wire_id).is_some() {
                    // Capability probe succeeded -- the verdict is now
                    // cached (`Prober::probed`). Nothing further to do here:
                    // the NEXT `recompile()` (triggered by any future demand
                    // change) is what actually routes a broad filter for
                    // this relay onto negentropy -- see the builder report's
                    // scoping note on already-open subs at probe time.
                } else if let Some(sub_id) = self.attribution.sub_id_for_wire(&relay, wire_id) {
                    self.step_neg_session(sub_id, relay.clone(), message.as_ref(), &mut effects);
                }
                // An unrecognized wire id is an untrusted-network fact
                // (stale/foreign sub), never a panic -- silently ignored,
                // same discipline as `handle_write_ack`'s unknown-OK case.
            }
            RelayMessage::NegErr {
                subscription_id, ..
            } => {
                let wire_id = subscription_id.as_str();
                if self.prober.on_neg_unsupported(&relay, wire_id) {
                    // Probe classified Unsupported; cached, never re-probed.
                } else if let Some(sub_id) = self.attribution.sub_id_for_wire(&relay, wire_id) {
                    if let Some(session) = self.neg_sessions.remove(&sub_id) {
                        self.neg_session_fallback_to_req(sub_id, session, &mut effects);
                    }
                }
            }
            // Closed/Notice/Auth/Count: AUTH-handshake territory, not built
            // in D/E (plan §7 non-goal unless a falsifier test forces it).
            _ => {}
        }
        effects
    }

    // ---- shared recompile + row-refresh plumbing -------------------------

    /// Recompile the router from the resolver's CURRENT demand, record any
    /// newly-sent REQs' attribution snapshots, and push `Effect::Wire` for
    /// whatever op actually changed on the wire -- EXCEPT a broad
    /// (unlimited) `Req` for a relay this reducer has PROVEN supports
    /// NIP-77 (`Prober::probed`), which is routed negentropy-first instead
    /// (plan §6 E: "negentropy-FIRST for a probed relay + broad filter; REQ
    /// fallback otherwise"). Ledger #8 is structural here, not a runtime
    /// `if` bolted on top: `open_neg_session` is the ONLY call site that can
    /// produce an `Effect::NegOpen`, and it can only be reached by first
    /// obtaining a `ProbedRelay` from `Prober::probed` -- an unprobed relay
    /// has no token to pass, so its `Req` arm always falls through to the
    /// plain-REQ branch below, every time.
    fn recompile(&mut self, effects: &mut Vec<Effect>) {
        self.sync_discovery();
        let demand = self.resolver.active_demand();
        self.attribution.observe_demand(demand.iter());
        let wire_delta: WireDelta = self
            .router
            .compile(&demand, self.directory.as_ref(), self.cap);
        // `router.compile()` above ALWAYS finalizes `prev_plan`/`last_diag`
        // for the full current demand, regardless of whether anything
        // actually changed on the wire (see `Router::compile`'s own body) —
        // so diagnostics is pushed unconditionally here (M5 plan §1.2 step
        // 3: "push it at the end of recompile()"), even on the early return
        // below for a no-op wire delta.
        effects.push(Effect::EmitDiagnostics(self.diagnostics_snapshot()));
        if wire_delta.ops.is_empty() {
            return;
        }

        let mut kept: Vec<(RelayUrl, Vec<WireOp>)> = Vec::new();
        for (relay, ops) in &wire_delta.ops {
            let mut kept_ops: Vec<WireOp> = Vec::new();
            for op in ops {
                match op {
                    WireOp::Req(sub_id, filter) => {
                        let absorbed = self
                            .router
                            .plan()
                            .reqs
                            .get(relay)
                            .and_then(|reqs| reqs.iter().find(|r| &r.sub_id == sub_id))
                            .map(|r| r.absorbed.clone())
                            .unwrap_or_default();

                        // "Small exact result" (a `limit`) always stays REQ
                        // -- a bounded, terminating fetch is not what
                        // negentropy set-reconciliation is for, and `limit`
                        // poisons coverage attribution regardless (ruling
                        // §3), so there is nothing negentropy-first would
                        // buy it.
                        let broad = filter.limit.is_none();
                        match (broad, self.prober.probed(relay)) {
                            (true, Some(probed)) => {
                                self.open_neg_session(
                                    probed,
                                    sub_id.clone(),
                                    filter.clone(),
                                    absorbed,
                                    effects,
                                );
                            }
                            _ => {
                                self.attribution
                                    .record_send(relay, sub_id, filter, absorbed);
                                kept_ops.push(op.clone());
                            }
                        }
                    }
                    WireOp::Close(sub_id) => {
                        self.neg_sessions.remove(sub_id);
                        kept_ops.push(op.clone());
                    }
                }
            }
            if !kept_ops.is_empty() {
                kept.push((relay.clone(), kept_ops));
            }
        }

        if !kept.is_empty() {
            effects.push(Effect::Wire(WireDelta { ops: kept }));
        }
    }

    /// The self-bootstrapping outbox (M5, `docs/known-gaps.md`'s
    /// "RelayDirectory" gap): keep an internal kind:10002 discovery
    /// subscription open covering EVERY author current demand has EVER
    /// referenced whose write relays `self.directory` didn't know yet at the
    /// time -- never a permanent/whole-graph scan (still bounded by "every
    /// author this session has actually demanded content for"). Called at
    /// the top of every `recompile` (i.e. on every subscribe/unsubscribe/
    /// re-root/ingest).
    ///
    /// WIDEN-ONLY (`docs/known-gaps.md`'s kind:10002 over-fetch finding: 7112
    /// events received against a 39-author resolved set, root-caused to THIS
    /// function -- see the finding's investigation notes): a newly-demanded
    /// author with unknown relays widens the subscription; an author whose
    /// relays just became known is deliberately left IN the filter rather
    /// than dropped. Reopening on every shrink was the actual bug -- an
    /// author leaving `needed` the moment their kind:10002 resolves used to
    /// tear down and reopen the ENTIRE subscription (dropping that one
    /// author from a fresh, differently-shaped filter), and to a NIP-01
    /// relay an overwriting Req on an already-open sub-id is
    /// indistinguishable from a brand-new subscription: it replies with a
    /// full EOSE replay of every event still matching the new filter. Over N
    /// authors resolving one at a time that is a triangular-number amount of
    /// redelivered events (N+(N-1)+...+1), not O(N) -- exactly the
    /// mechanism behind the 7112-for-39 finding. Leaving a resolved author
    /// in the filter a while longer is widen-safe (matches(wider) ⊇
    /// matches(narrower), the same proof obligation `nmp_router::coalesce`'s
    /// `AuthorUnion` rule already carries) -- it can only mean a few extra,
    /// already-known kind:10002 deliveries for that author, never a
    /// structural over-fetch. The subscription is only ever torn down when
    /// `needed` goes fully empty (every demanded author has resolved, or
    /// none are demanded at all) -- at that point there is nothing left this
    /// discovery sub is for, so it closes rather than idling forever.
    ///
    /// Deliberately reuses the ordinary resolver subscribe/unsubscribe
    /// machinery rather than hand-rolling a parallel subscription system:
    /// the discovery atom this produces (`kinds:[10002], authors:{covered}`)
    /// is just another entry in `resolver.active_demand()`, so the router's
    /// EXISTING discovery-kind eligibility is what routes it to the
    /// configured indexers -- no router-side change was needed for that half
    /// at all. A content atom for an author with no known write relays
    /// simply routes nowhere in the meantime (never an indexer fallback --
    /// "indexers are never a content fallback").
    fn sync_discovery(&mut self) {
        let needed: BTreeSet<PubkeyHex> = self
            .resolver
            .active_demand()
            .into_iter()
            .filter_map(|atom| atom.authors)
            .flatten()
            // NOT `write_relays(..).is_empty()`: that collapses "known,
            // declares zero write relays" into the same signal as "never
            // resolved", which kept a discovery subscription open FOREVER
            // for an author who genuinely has no write relays (ledger #20).
            // `knows_write_relays` distinguishes the two; only a genuinely
            // unresolved author still needs discovery.
            .filter(|author| !self.directory.knows_write_relays(author))
            .collect();

        if needed.is_empty() {
            if self.discovery_handle.is_none() && self.discovery_authors.is_empty() {
                return; // already closed -- nothing to do.
            }
            // Every previously-needed author has resolved (or nothing was
            // ever demanded): nothing left for this sub to cover, so close
            // it. Its `Drop` impl only ENQUEUES the withdrawal; there is
            // nothing to replace it with, so flush explicitly.
            self.discovery_handle = None;
            self.discovery_authors = BTreeSet::new();
            let _ = self.resolver.poll_pending_drops();
            return;
        }

        if needed.is_subset(&self.discovery_authors) {
            // Nothing NEW to cover -- leave the existing subscription
            // exactly as-is, even though it may now be wider than strictly
            // required (see this fn's doc: that's the whole point).
            return;
        }

        // Widen: union in whatever's newly needed and reopen with the
        // WIDENED set. Its `Drop` impl only ENQUEUES the old withdrawal;
        // `resolver.subscribe`'s own drain-on-entry flushes it before
        // building the new atom.
        self.discovery_authors = self.discovery_authors.union(&needed).cloned().collect();
        self.discovery_handle = None;
        let query = LiveQuery(Filter {
            kinds: Some(BTreeSet::from([NIP65_RELAY_LIST_KIND])),
            authors: Some(Binding::Literal(self.discovery_authors.clone())),
            ..Filter::default()
        });
        let (handle, _delta) = self.resolver.subscribe(query);
        self.discovery_handle = Some(handle);
    }

    /// After ingesting a possible kind:10002 event for `author`, re-read the
    /// store's CURRENT winning relay-list event for them -- never trust the
    /// just-arrived frame directly. `EventStore::query` only ever returns
    /// the current replaceable-event winner (`nmp-store`'s own contract), so
    /// this is correct regardless of cross-relay arrival order: a stale/
    /// older copy that already lost the replaceable race at `insert` time
    /// can never overwrite the directory with worse data than what the
    /// store itself considers authoritative.
    fn ingest_relay_list_winner(&mut self, author: nostr::PublicKey) {
        let filter = ConcreteFilter {
            kinds: Some(BTreeSet::from([NIP65_RELAY_LIST_KIND])),
            authors: Some(BTreeSet::from([author.to_hex()])),
            ..ConcreteFilter::default()
        };
        let Some(winner) = self
            .resolver
            .store()
            .query(&filter.to_nostr())
            .into_iter()
            .next()
        else {
            return;
        };
        let write_relays = parse_nip65_write_relays(&winner.event);
        self.directory
            .ingest_write_relays(author.to_hex(), write_relays);
        let read_relays = parse_nip65_read_relays(&winner.event);
        self.directory
            .ingest_read_relays(author.to_hex(), read_relays);
    }

    /// Open a real negentropy reconciliation for `filter` against `probed`
    /// (plan §6 E). Reads the local store's own current holdings for the
    /// (window-erased) shape to seed the `Reconciler`, records the send-time
    /// attribution snapshot exactly as a plain REQ would (ruling §2: NEG
    /// runs unfloored/unlimited, so `neg_filter` below IS that snapshot's
    /// filter, with no separate floor/until/limited fields to keep in
    /// sync), and emits the `NegOpen` effect.
    fn open_neg_session(
        &mut self,
        probed: ProbedRelay,
        sub_id: SubId,
        filter: ConcreteFilter,
        absorbed: BTreeSet<CoverageKey>,
        effects: &mut Vec<Effect>,
    ) {
        // REQ and NEG-OPEN share ONE subscription-id namespace on the wire
        // (NIP-77): release whatever this `sub_id` may already mean to the
        // relay (a live plain REQ from before this relay was known
        // `Supported`, or nothing at all -- closing an id the relay never
        // opened is a harmless no-op) before reopening it as a NEG session.
        effects.push(Effect::Wire(WireDelta {
            ops: vec![(probed.url().clone(), vec![WireOp::Close(sub_id.clone())])],
        }));

        let neg_filter = ConcreteFilter {
            since: None,
            until: None,
            limit: None,
            ..filter
        };
        let local_ids: Vec<(u64, EventId)> = self
            .resolver
            .store()
            .query(&neg_filter.to_nostr())
            .into_iter()
            .map(|se| (se.event.created_at.as_secs(), se.event.id))
            .collect();
        let (reconciler, initial_hex) = Reconciler::open(&local_ids);

        self.attribution
            .record_send(probed.url(), &sub_id, &neg_filter, absorbed.clone());
        self.neg_sessions.insert(
            sub_id.clone(),
            NegSession {
                relay: probed.url().clone(),
                filter: neg_filter.clone(),
                absorbed,
                started_at: self.clock,
                reconciler,
            },
        );
        effects.push(Effect::NegOpen(probed, sub_id, neg_filter, initial_hex));
    }

    /// Drive one inbound `NEG-MSG` round for `sub_id`'s live session, if any
    /// (a frame for a sub this reducer isn't tracking is an untrusted-
    /// network fact, silently ignored -- same discipline as
    /// `handle_write_ack`'s unknown-`OK` case).
    fn step_neg_session(
        &mut self,
        sub_id: SubId,
        relay: RelayUrl,
        message_hex: &str,
        effects: &mut Vec<Effect>,
    ) {
        let Some(session) = self.neg_sessions.get_mut(&sub_id) else {
            return;
        };
        let step = session.reconciler.step(message_hex);
        match step {
            Ok(NegStep::Continue(next_hex)) => {
                effects.push(Effect::NegMsg(relay, sub_id, next_hex));
            }
            Ok(NegStep::Done(need_ids)) => {
                let session = self
                    .neg_sessions
                    .remove(&sub_id)
                    .expect("just matched via get_mut above -- still present");
                self.finish_neg_session(sub_id, relay, session, need_ids, effects);
            }
            Err(_) => {
                // A malformed/unexpected reconcile payload from an
                // untrusted relay: abandon this reconciliation and fall
                // back to a plain REQ for the same filter -- the same
                // recovery path as the liveness-deadline/NEG-ERR cases,
                // never a silent read-gap.
                if let Some(session) = self.neg_sessions.remove(&sub_id) {
                    self.neg_session_fallback_to_req(sub_id, session, effects);
                }
            }
        }
    }

    /// Reconciliation completed (plan §6 E, the ruling's "feed a NEG-DONE
    /// the same way [as EOSE]"). Releases the session's sub-id, backfills
    /// whatever ids negentropy proved we are missing through the ordinary
    /// REQ/EOSE/ingest pipeline (never a separate ingest path), and reopens
    /// the same sub-id as a plain, live REQ floored at "now" -- negentropy
    /// is a point-in-time backlog sync, not a persistent subscription
    /// (ruling §3), so the relay's ongoing live tail still needs an open
    /// REQ once the backlog is settled.
    ///
    /// Evidence crediting (ledger #7) is NOT immediate when a backfill is
    /// needed: recording a reconciled watermark before the backfilled events
    /// are actually ingested would attach evidence to a store
    /// that is still, transiently, missing precisely the events negentropy
    /// just proved are missing.
    /// `pending_neg_credit` defers the credit to the backfill sub's OWN
    /// EOSE (`on_relay_frame`), by which point the events are already
    /// ingested (EVENT frames precede EOSE, NIP-01). An empty `need_ids`
    /// has nothing to wait for, so it credits right away.
    fn finish_neg_session(
        &mut self,
        sub_id: SubId,
        relay: RelayUrl,
        session: NegSession,
        need_ids: BTreeSet<EventId>,
        effects: &mut Vec<Effect>,
    ) {
        let NegSession {
            filter, absorbed, ..
        } = session;
        effects.push(Effect::NegClose(relay.clone(), sub_id.clone()));

        if need_ids.is_empty() {
            self.credit_neg_coverage(&sub_id, &relay, effects);
        } else {
            let backfill = ConcreteFilter {
                ids: Some(need_ids.iter().map(|id| id.to_hex()).collect()),
                ..ConcreteFilter::default()
            };
            let backfill_sub = SubId::for_filter(relay.clone(), &backfill);
            self.pending_backfills.insert(backfill_sub.clone());
            self.pending_neg_credit
                .insert(backfill_sub.clone(), sub_id.clone());
            // No coverage credit of its OWN for this one-shot id-set fetch
            // -- `absorbed` is deliberately empty; it targets exactly the
            // ids negentropy already proved, it is not itself a proof over
            // any atom's shape (the credit it unlocks is `sub_id`'s, via
            // `pending_neg_credit` above).
            self.attribution
                .record_send(&relay, &backfill_sub, &backfill, BTreeSet::new());
            effects.push(Effect::Wire(WireDelta {
                ops: vec![(relay.clone(), vec![WireOp::Req(backfill_sub, backfill)])],
            }));
        }

        let live_tail = ConcreteFilter {
            since: Some(self.clock.as_secs()),
            ..filter
        };
        self.attribution
            .record_send(&relay, &sub_id, &live_tail, absorbed);
        effects.push(Effect::Wire(WireDelta {
            ops: vec![(relay, vec![WireOp::Req(sub_id, live_tail)])],
        }));
    }

    /// Attribute coverage for `sub_id` through the EXACT SAME
    /// `AttributionState::attribute_eose` call the real EOSE path uses --
    /// no second coverage mechanism, whether called directly (no backfill
    /// needed) or from `on_relay_frame`'s EOSE arm once a deferred backfill
    /// lands (`pending_neg_credit`).
    fn credit_neg_coverage(&mut self, sub_id: &SubId, relay: &RelayUrl, effects: &mut Vec<Effect>) {
        let attributed =
            self.attribution
                .attribute_eose(relay, &wire_sub_id_string(sub_id), self.clock);
        for (key, interval) in attributed {
            if let Some(shape) = self.attribution.shape_of(key) {
                self.resolver
                    .store_mut()
                    .record_coverage(&shape, relay, interval);
                effects.push(Effect::RecordCoverage(key, relay.clone(), interval));
            }
        }
        self.refresh_all_handles(effects);
    }

    /// Abandon a live reconciliation and fall back to a plain REQ for the
    /// SAME (unfloored/unlimited) filter -- shared by the liveness-deadline
    /// sweep (`tick`), an inbound `NEG-ERR`, and a malformed reconcile
    /// payload (`step_neg_session`'s `Err` arm). The abandoned session's own
    /// attribution snapshot is left outstanding rather than popped: the
    /// fallback REQ's EOSE will credit it via the SAME intersection rule an
    /// overwriting REQ already relies on (both snapshots carry the
    /// identical `absorbed`/`floor`/`until`/`limited` fields, since both
    /// derive from `session.filter`), so pop order does not matter here.
    fn neg_session_fallback_to_req(
        &mut self,
        sub_id: SubId,
        session: NegSession,
        effects: &mut Vec<Effect>,
    ) {
        effects.push(Effect::NegClose(session.relay.clone(), sub_id.clone()));
        self.attribution.record_send(
            &session.relay,
            &sub_id,
            &session.filter,
            session.absorbed.clone(),
        );
        effects.push(Effect::Wire(WireDelta {
            ops: vec![(session.relay, vec![WireOp::Req(sub_id, session.filter)])],
        }));
    }

    fn refresh_all_handles(&mut self, effects: &mut Vec<Effect>) {
        let ids: Vec<HandleId> = self.handles.keys().copied().collect();
        for id in ids {
            self.refresh_handle(id, effects);
        }
    }

    /// Recompute `id`'s current row set + acquisition evidence; emit (and
    /// synchronously deliver to its sink) `Effect::EmitRows` only if either
    /// changed since the last refresh -- and, when something DID change, the
    /// row payload is ALWAYS just the incremental added/removed delta
    /// against `state.last_rows`, never the full current set (see
    /// `RowDelta`'s doc: this is what keeps a long-running subscription's
    /// total delivered row volume ~O(distinct rows) instead of O(rows²)).
    /// Evidence can change with no row change at all (a watermark advancing,
    /// or a source's link status flipping) -- that case still emits,
    /// carrying an EMPTY row delta alongside the new evidence.
    fn refresh_handle(&mut self, id: HandleId, effects: &mut Vec<Effect>) {
        let (current, evidence) = self.rows_and_evidence_for(id);
        let Some(state) = self.handles.get_mut(&id) else {
            return;
        };
        let current_ids: BTreeSet<EventId> = current.keys().copied().collect();
        if current_ids == state.last_rows && state.last_evidence.as_ref() == Some(&evidence) {
            return;
        }
        let mut delta: Vec<RowDelta> = Vec::new();
        for (event_id, event) in &current {
            if !state.last_rows.contains(event_id) {
                delta.push(RowDelta::Added(event.clone()));
            }
        }
        for old_id in &state.last_rows {
            if !current.contains_key(old_id) {
                delta.push(RowDelta::Removed(*old_id));
            }
        }
        state.last_rows = current_ids;
        state.last_evidence = Some(evidence.clone());
        state.sink.on_rows(delta.clone());
        effects.push(Effect::EmitRows(id, delta, evidence));
    }

    /// The query's FULL current matching row set (by id) + its
    /// [`AcquisitionEvidence`] -- an internal snapshot `refresh_handle`
    /// diffs against the handle's own remembered `last_rows` to compute the
    /// outgoing delta. This snapshot itself is never handed to a caller/
    /// effect directly. Rows are computed over `root_atoms` alone (delivery
    /// shape unchanged); evidence is computed over `subtree_atoms` (#12: the
    /// query's FULL subtree, interior `Derived` atoms included).
    fn rows_and_evidence_for(
        &self,
        id: HandleId,
    ) -> (BTreeMap<EventId, nostr::Event>, AcquisitionEvidence) {
        let root_atoms = self.resolver.root_atoms(id);
        let mut by_id: BTreeMap<EventId, nostr::Event> = BTreeMap::new();
        for atom in &root_atoms {
            for se in self.resolver.store().query(&atom.to_nostr()) {
                by_id.entry(se.event.id).or_insert(se.event);
            }
        }
        let subtree_atoms = self.resolver.subtree_atoms(id);
        let evidence = evidence::acquisition_evidence(
            &subtree_atoms,
            self.router.plan(),
            self.resolver.store(),
            &self.connected_relays,
            &self.ever_connected_relays,
        );
        (by_id, evidence)
    }
}

/// Parse NIP-65 `r` tags off a kind:10002 event into its WRITE relay set
/// (lane `Nip65Write`): an absent marker or an explicit `"write"` marker is
/// a write relay; an explicit `"read"` marker is excluded. Mirrors
/// `nmp-demo`'s former one-shot bootstrap parse exactly (the same NIP-65
/// semantics), now run reactively per event instead of once up front.
fn parse_nip65_write_relays(event: &nostr::Event) -> Vec<LanedRelay> {
    event
        .tags
        .iter()
        .filter_map(|t| {
            let s = t.as_slice();
            if s.first().map(String::as_str) != Some("r") {
                return None;
            }
            let url = RelayUrl::parse(s.get(1)?).ok()?;
            match s.get(2).map(String::as_str) {
                Some("read") => None,
                _ => Some(LanedRelay::new(url, Lane::Nip65Write)),
            }
        })
        .collect()
}

/// Parse NIP-65 `r` tags off a kind:10002 event into its READ relay set
/// (lane `Nip65Read`): the mirror of `parse_nip65_write_relays` -- an
/// absent marker or an explicit `"read"` marker is a read relay; an
/// explicit `"write"` marker is excluded (`routing-and-ownership.md` §2.4 --
/// an unmarked `r` tag counts as BOTH read and write, per NIP-65).
fn parse_nip65_read_relays(event: &nostr::Event) -> Vec<LanedRelay> {
    event
        .tags
        .iter()
        .filter_map(|t| {
            let s = t.as_slice();
            if s.first().map(String::as_str) != Some("r") {
                return None;
            }
            let url = RelayUrl::parse(s.get(1)?).ok()?;
            match s.get(2).map(String::as_str) {
                Some("write") => None,
                _ => Some(LanedRelay::new(url, Lane::Nip65Read)),
            }
        })
        .collect()
}

#[cfg(test)]
mod nip65_read_write_split_tests {
    //! Unit A's NIP-65 read/write parse split (`routing-and-ownership.md`
    //! §2.4) -- private free functions, so tested directly in-module rather
    //! than via the heavier `tests/self_bootstrap_outbox.rs`-style engine
    //! harness (which already covers `parse_nip65_write_relays` end-to-end
    //! via `relay_list_parse_excludes_explicit_read_only_relays`).

    use nmp_router::LiveDirectory;
    use nmp_store::MemoryStore;
    use nmp_transport::RelayFrame;
    use nostr::nips::nip65::RelayMetadata;
    use nostr::{EventBuilder, JsonUtil, Keys, Kind, RelayMessage, SubscriptionId, Tag, Tags};

    use super::*;

    fn relay_list_event(author: &Keys, tags: Vec<Tag>) -> nostr::Event {
        EventBuilder::new(Kind::RelayList, "")
            .tags(Tags::from_list(tags))
            .sign_with_keys(author)
            .expect("test fixture event must sign cleanly")
    }

    #[test]
    fn nip65_unmarked_relay_is_both_read_and_write() {
        let author = Keys::generate();
        let r = RelayUrl::parse("wss://both.example.com").unwrap();
        let event = relay_list_event(&author, vec![Tag::relay_metadata(r.clone(), None)]);

        assert_eq!(
            parse_nip65_write_relays(&event),
            vec![LanedRelay::new(r.clone(), Lane::Nip65Write)],
            "an unmarked r tag must count as a write relay"
        );
        assert_eq!(
            parse_nip65_read_relays(&event),
            vec![LanedRelay::new(r, Lane::Nip65Read)],
            "an unmarked r tag must ALSO count as a read relay (NIP-65: unmarked = both)"
        );
    }

    #[test]
    fn nip65_write_marked_excluded_from_read() {
        let author = Keys::generate();
        let r = RelayUrl::parse("wss://write-only.example.com").unwrap();
        let event = relay_list_event(
            &author,
            vec![Tag::relay_metadata(r.clone(), Some(RelayMetadata::Write))],
        );

        assert_eq!(
            parse_nip65_write_relays(&event),
            vec![LanedRelay::new(r, Lane::Nip65Write)],
            "an explicit write-marked relay must still be a write relay"
        );
        assert!(
            parse_nip65_read_relays(&event).is_empty(),
            "an explicit write-marked relay must be excluded from the read set"
        );
    }

    #[test]
    fn nip65_read_marked_excluded_from_write() {
        let author = Keys::generate();
        let r = RelayUrl::parse("wss://read-only.example.com").unwrap();
        let event = relay_list_event(
            &author,
            vec![Tag::relay_metadata(r.clone(), Some(RelayMetadata::Read))],
        );

        assert!(
            parse_nip65_write_relays(&event).is_empty(),
            "an explicit read-marked relay must be excluded from the write set"
        );
        assert_eq!(
            parse_nip65_read_relays(&event),
            vec![LanedRelay::new(r, Lane::Nip65Read)],
            "an explicit read-marked relay must still be a read relay"
        );
    }

    /// `ingest_relay_list_winner` stores BOTH sets from the ONE kind:10002
    /// winner in a single pass (`routing-and-ownership.md` §2.4) -- proven
    /// through the real `EngineCore::on_relay_frame` path (not a bypassed
    /// direct directory poke), against a relay list mixing an unmarked
    /// (both), an explicit write-only, and an explicit read-only relay.
    #[test]
    fn live_directory_stores_read_and_write_from_one_winner() {
        let author = Keys::generate();
        let relay_url = RelayUrl::parse("wss://relay.example.com").unwrap();
        let both = RelayUrl::parse("wss://both.example.com").unwrap();
        let write_only = RelayUrl::parse("wss://write-only.example.com").unwrap();
        let read_only = RelayUrl::parse("wss://read-only.example.com").unwrap();

        let dir = LiveDirectory::builder().build();
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(dir), 10);

        core.handle(EngineMsg::RelayConnected(
            TransportRelayHandle {
                slot: 0,
                generation: 1,
            },
            relay_url.clone(),
        ));

        let event = relay_list_event(
            &author,
            vec![
                Tag::relay_metadata(both.clone(), None),
                Tag::relay_metadata(write_only.clone(), Some(RelayMetadata::Write)),
                Tag::relay_metadata(read_only.clone(), Some(RelayMetadata::Read)),
            ],
        );
        core.handle(EngineMsg::RelayFrame(
            TransportRelayHandle {
                slot: 0,
                generation: 1,
            },
            RelayFrame::Text(RelayMessage::event(SubscriptionId::new("s"), event).as_json()),
        ));

        let author_hex = author.public_key().to_hex();
        let write_relays: BTreeSet<RelayUrl> = core
            .directory
            .write_relays(&author_hex)
            .into_iter()
            .map(|lr| lr.url)
            .collect();
        let read_relays: BTreeSet<RelayUrl> = core
            .directory
            .read_relays(&author_hex)
            .into_iter()
            .map(|lr| lr.url)
            .collect();

        assert_eq!(
            write_relays,
            BTreeSet::from([both.clone(), write_only.clone()]),
            "write set must be {{unmarked, write-marked}}, excluding read-marked"
        );
        assert_eq!(
            read_relays,
            BTreeSet::from([both, read_only]),
            "read set must be {{unmarked, read-marked}}, excluding write-marked"
        );
    }
}
