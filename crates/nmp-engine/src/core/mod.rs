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

mod admission;
mod attribution;
mod diagnostics;
mod evidence;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;

#[cfg(test)]
use std::cell::Cell;

use nostr::{
    Event as SignedEvent, EventId, PublicKey, RelayMessage, RelayUrl, Timestamp, UnsignedEvent,
};

use nmp_grammar::{
    AccessContext, Binding, CacheMode, ConcreteFilter, ContextualAtom, Durability, Filter,
    HostAuthority, NarrowOnly, PrivateRoute, SourceAuthority, WriteIntent, WritePayload,
    WriteRouting,
};
use nmp_resolver::{Engine as ResolverEngine, HandleId, LiveQuery, QueryHandle};
use nmp_router::{
    DiscoveryKinds, Lane, LanedRelay, PubkeyHex, RelayDirectory, RelayLimits, Router, RuleRegistry,
    SubId, WireDelta, WireOp, WireReq,
};
use nmp_signer::SignerError;
use nmp_store::{
    sentinel_signature, AcceptOutcome, AcceptWrite, AttemptOutcome, CompensateOutcome, CoverageKey,
    EventStore, IntentId, IntentSigState, PersistenceError, PromoteOutcome, ReceiptState,
    RelayObserved, WriteDurability,
};
use nmp_transport::{
    AttemptCorrelation, HandoffResult, RelayFrame, RelayHandle as TransportRelayHandle, RelayHealth,
};

use crate::negentropy::{NegStep, ProbedRelay, Prober, Reconciler};
use crate::outbox::{ReceiptSink, WriteStatus};

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

pub use admission::RelayAdmissionPolicy;
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

/// A publish failure that occurs before any receipt identity can exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishError {
    /// Every upper-half correlation id has already been issued. No id is
    /// reused, wrapped into the durable lower half, or fabricated.
    ReceiptCorrelationIdExhausted,
}

impl std::fmt::Display for PublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReceiptCorrelationIdExhausted => {
                write!(f, "receipt correlation id namespace exhausted")
            }
        }
    }
}

impl std::error::Error for PublishError {}

/// Truthful result of trying to attach a receipt observer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReattachOutcome {
    /// The retained receipt and all replay evidence were readable; the sink
    /// was primed and, for live work, registered for subsequent facts.
    Attached,
    /// This store has no retained receipt with the requested id.
    NotFound,
    /// The receipt identity is retained, but its receipt/attempt/route evidence
    /// cannot be decoded. Nothing is published, deleted, or attached.
    RetainedButUnreadable,
}

impl ReattachOutcome {
    pub fn is_attached(self) -> bool {
        self == Self::Attached
    }
}

/// Sink an app-facing `Handle` registers for row deltas on a subscription.
pub trait RowSink: Send {
    fn on_rows(&self, rows: Vec<RowDelta>);
}

/// The canonical row value (#105): the event plus its sorted, deduplicated
/// relay-observation set -- `nmp_store::Provenance::seen`'s keys, projected
/// honestly rather than mirrored into a second parallel provenance store.
/// `sources` only ever grows for a given event id (`Provenance::
/// merge_observation` never removes an entry), so `Row`/`RowDelta` never
/// need a "sources shrank" case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub event: nostr::Event,
    pub sources: BTreeSet<RelayUrl>,
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
    /// A row that newly matches the query, carrying the full row (event +
    /// its current relay-provenance set) so the app never has to look
    /// either up separately.
    Added(Row),
    /// The SAME row already matched (#105): its relay-provenance SET grew --
    /// a relay not already in it delivered this exact event id. This is a
    /// `BTreeSet<RelayUrl>` compare, not a timestamp compare: an
    /// already-seen relay redelivering at a strictly later timestamp DOES
    /// advance `nmp_store::Provenance::merge_observation`'s internal
    /// watermark, but the projected SET is unchanged, so it correctly does
    /// NOT fire this variant (the "no spurious update for an identical
    /// observation" bar applies to the set, which is all this surface ever
    /// exposes). The event body itself is unchanged, so only the id and the
    /// row's FULL current source set are carried (matching `Added`'s own
    /// "whole value, not a patch" shape) -- never fired for a no-op
    /// redelivery, and never fired merely because SOME OTHER handle's
    /// lifecycle event forced a `refresh_handle` recompute of this one.
    SourcesGrew {
        id: EventId,
        sources: BTreeSet<RelayUrl>,
    },
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
            RowDelta::Added(row) => row.event.id,
            RowDelta::SourcesGrew { id, .. } => *id,
            RowDelta::Removed(id) => *id,
        }
    }

    /// The event payload, if this is an `Added` delta (`None` for
    /// `SourcesGrew`/`Removed` -- the app is expected to already hold the
    /// event from an earlier `Added`).
    pub fn event(&self) -> Option<&nostr::Event> {
        match self {
            RowDelta::Added(row) => Some(&row.event),
            RowDelta::SourcesGrew { .. } | RowDelta::Removed(_) => None,
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
    RelayHealth(u32, RelayHealth),
    RelayFrame(TransportRelayHandle, RelayFrame),
    RelayFrames(Vec<(TransportRelayHandle, RelayFrame)>),
    SignerCompleted(ReceiptId, u64, Result<SignedEvent, SignerError>),
    /// The runtime has no signer attached for this accepted author. This is
    /// non-terminal: the canonical pending row and durable obligation stay
    /// alive until a matching signer is attached or the app cancels.
    SignerUnavailable(ReceiptId, u64),
    /// A capability for this author was attached. Re-arm every matching
    /// accepted unsigned intent through the ordinary RequestSign effect.
    SignerAttached(PublicKey),
    /// Explicit pre-signature cancellation. Once promotion has committed,
    /// cancellation cannot retract a valid signed cache row.
    CancelWrite(ReceiptId),
    /// The one, ever, typed result of a durable `EVENT` handoff (issue
    /// #93), translated from `PoolEvent::EventHandoff`. See
    /// `EngineCore::on_event_handoff`'s doc for what this does and does
    /// NOT do in this unit.
    EventHandoff(AttemptCorrelation, HandoffResult),
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
    /// The publish could not even allocate a non-durable correlation id,
    /// so no `EmitReceipt` can truthfully accompany this failure.
    PublishFailed(PublishError),
    RequestSign(ReceiptId, u64, UnsignedEvent),
    RequestDecrypt(EventId, PublicKey, String),
    /// Outbox: publish `event` to `relay` (plan §3.4's "`Effect::Wire`
    /// publish REQ/EVENT per relay", re-cut as its OWN effect rather than a
    /// `nmp_router::WireOp` variant — `WireOp`/`WireDelta` are read-
    /// subscription vocabulary owned by `nmp-router`, out of this builder's
    /// scope to extend; this is engine-owned wire vocabulary for the write
    /// plane). C (runtime) translates this to `Pool::send_durable` of an
    /// `["EVENT", …]` frame on `relay`'s current generation, correlated by
    /// `AttemptCorrelation` (issue #93) — the durable handoff is generation-
    /// scoped and reports back exactly one typed `HandoffResult`, never
    /// silently carried into a later connection.
    PublishEvent(RelayUrl, SignedEvent, AttemptCorrelation),
}

/// Per-handle bookkeeping `EngineCore` must retain across `handle()` calls:
/// the `QueryHandle` itself (dropping it would withdraw the subscription —
/// see `nmp_resolver::QueryHandle`'s `Drop` impl), the app-facing sink, and
/// the last-emitted row/evidence state (so `EmitRows` fires only when
/// something actually changed, not on every unrelated recompile).
/// `AcquisitionEvidence` derives `PartialEq` precisely so this
/// change-detection compare stays a plain value comparison, as the former
/// query-evidence aggregate's did. `last_rows` maps each currently-matching
/// id to the SOURCE SET last emitted for it (#105) -- not just the id --
/// so `refresh_handle` can detect provenance growth on an already-matching
/// row the SAME way it already detects `Added`/`Removed`: a plain value
/// compare against this remembered state, never a second bespoke mechanism.
struct HandleState {
    _handle: QueryHandle,
    sink: Box<dyn RowSink>,
    last_rows: BTreeMap<EventId, BTreeSet<RelayUrl>>,
    last_evidence: Option<AcquisitionEvidence>,
}

/// Per-receipt bookkeeping the reducer retains from `Publish` through to the
/// last per-relay ack (or `Ephemeral`'s generation-scoped handoff effects).
/// Ephemeral still owns a receipt-only record and status stream; what it
/// lacks is a durable delivery obligation and canonical pending row.
struct PendingWrite {
    durability: Durability,
    routing: WriteRouting,
    /// False only when a persisted routing snapshot cannot be decoded.
    /// Recovery keeps owning the obligation but fails closed on wire output.
    routing_valid: bool,
    /// Zero or more observers. Recovery owns the obligation even before an
    /// app reattaches, and multiple observers may follow the same receipt.
    sinks: Vec<Rc<dyn ReceiptSink>>,
    /// Store-allocated durable intent id. `None` only for Ephemeral's
    /// receipt-only path, which never owns a pending row.
    intent_id: Option<IntentId>,
    /// Signer identity selected and frozen at acceptance. Later active-
    /// account changes cannot redirect this obligation.
    signing_pubkey: PublicKey,
    /// Exact frozen body accepted by the store (sentinel signature). Kept
    /// so signer responses can be validated byte-for-byte before promotion
    /// and so compensation can invalidate the ordinary resolver graph.
    frozen: SignedEvent,
    /// True when `accept_write` found an already-signed duplicate and
    /// journaled this co-owner as Signed immediately.
    already_signed: bool,
    /// Exactly one signer operation may be outstanding for an intent.
    /// Attach/activate notifications are idempotent while this is true.
    sign_request_in_flight: bool,
    sign_generation: u64,
    /// Set once the signer resolves; used to clean up `event_to_receipt`.
    event_id: Option<EventId>,
    /// Relays sent-to but not yet terminal (acked/rejected/given-up).
    /// Durable and AtMostOnce both populate this (both track real per-relay
    /// state); AtMostOnce's distinguishing property is that NOTHING in this
    /// reducer ever re-sends on a `RelayDisconnected` for either class — a
    /// dropped pending relay always resolves to `GaveUp`, never a retry
    /// `PublishEvent` (no blind retry, ledger's `AtMostOnce` amendment).
    pending_relays: BTreeSet<RelayUrl>,
    /// Routed lanes for which `start_attempt` failed. They remain explicitly
    /// owned and nonterminal, but never enter `pending_relays` because no
    /// Started fact exists and no wire EVENT was emitted.
    unstarted_relays: BTreeSet<RelayUrl>,
    /// Resolved URLs whose route revision did not persist. Owned only for
    /// this process lifetime; crash recovery may re-resolve policy but cannot
    /// claim these exact URLs durably.
    route_blocked_relays: BTreeSet<RelayUrl>,
    /// The persisted started ordinal currently awaiting a terminal outcome
    /// for each relay.
    attempt_ordinals: BTreeMap<RelayUrl, u64>,
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
    active_pubkey: Option<PublicKey>,
    /// Correlation ids for failures that were never accepted use the upper
    /// half of the namespace. Store-issued durable ids occupy the lower half
    /// and advance independently, so reattachment can never alias one.
    next_unaccepted_receipt: Option<u64>,
    /// Write outbox (§3.4 / VISION §7 ledger #6/#9). `pending` is keyed by
    /// `ReceiptId` from `Publish` through to the last terminal per-relay
    /// status; `event_to_receipt` lets an inbound `OK` frame (keyed by
    /// `EventId` on the wire) find its receipt.
    pending: HashMap<ReceiptId, PendingWrite>,
    event_to_receipts: HashMap<EventId, BTreeSet<ReceiptId>>,
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
    /// Next transport-native [`AttemptCorrelation`] to mint (issue #93).
    /// Purely volatile/in-process — never persisted, never restart-durable
    /// (the plan's own words: "no persistence migration" for this unit).
    /// Checked, typed exhaustion, same discipline as
    /// `next_unaccepted_receipt` above.
    next_attempt_correlation: Option<u64>,
    /// `AttemptCorrelation` -> which receipt/relay it was minted for. Engine-
    /// owned bookkeeping only; transport never needs to understand this
    /// mapping, only echo the correlation back unchanged. An entry is
    /// removed the instant its one-and-only `HandoffResult` arrives — see
    /// `Self::on_event_handoff`.
    attempt_correlations: HashMap<AttemptCorrelation, AttemptCorrelationTarget>,
    /// The provenance-aware relay admission policy for DISCOVERED relays
    /// (issue #121). Applied in [`Self::ingest_relay_list_winner`], the one
    /// choke point where a kind:10002 winner's relays become routable lanes.
    /// Defaults to the secure policy (reject every discovered private/
    /// loopback/onion host); production threads the operator's opt-in local
    /// allowlist via [`Self::with_relay_admission`].
    admission: RelayAdmissionPolicy,
    /// Monotonic count of DISCOVERED relay-lane rejections by `admission`
    /// before they could become directory lanes (issue #121). Counted PER
    /// LANE: `ingest_relay_list_winner` filters the write set and the read
    /// set of one kind:10002 separately (§2.4), so one hostile event naming
    /// `N` rejected hosts bumps this by up to `2N`. Surfaced in
    /// [`DiagnosticsSnapshot::discovered_private_relays_rejected`]; the
    /// separate worker-exhaustion cap count lives in the pool
    /// (`nmp_transport::Pool::admission_rejections`) and is folded in by the
    /// runtime.
    discovered_private_relays_rejected: u64,
    /// Read-only degrade flag (issue #122): set once the first time an
    /// ingest/read [`EventStore`] door returns [`PersistenceError`] (disk
    /// full, I/O error). The reducer NEVER panics on such a failure — it
    /// records the error message here, skips the affected reactive step
    /// (leaving already-delivered state untouched rather than fabricating a
    /// phantom retraction), and surfaces it on the read-only diagnostics
    /// snapshot. A minimal, honest "the local cache went read-only" signal;
    /// a richer failure-mode framework (recovery, reopen, per-door policy)
    /// is deliberately out of scope — see the issue's priority note.
    ///
    /// This flag is OBSERVATIONAL, not a gate: no code path reads it to
    /// refuse work. "Read-only" is descriptive — a later message simply
    /// re-attempts the same door and degrades again on a repeat failure
    /// (harmless: every widened door is atomic, so a failed attempt commits
    /// nothing). Enforcing degrade (short-circuiting further writes) would be
    /// the richer policy explicitly deferred here.
    store_degraded: Option<String>,
    transport_degraded: Option<String>,
    /// Test-only work counters for the affected-handle invalidation
    /// falsifier. Production pays no field or increment cost.
    #[cfg(test)]
    projection_store_queries: Cell<u64>,
    #[cfg(test)]
    router_compiles: Cell<u64>,
}

/// What one `AttemptCorrelation` (issue #93) resolves back to in this
/// reducer's own bookkeeping.
struct AttemptCorrelationTarget {
    receipt: ReceiptId,
    relay: RelayUrl,
    /// Ephemeral drops its `PendingWrite` immediately after producing the
    /// handoff effects, so each correlation snapshots the observer set that
    /// must still receive a truthful async `Sent`. Durable/AtMostOnce leave
    /// this empty and continue to notify through their retained pending row.
    ephemeral_sinks: Vec<Rc<dyn ReceiptSink>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AttemptCorrelationExhausted;

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
            active_pubkey: None,
            next_unaccepted_receipt: Some(u64::MAX),
            pending: HashMap::new(),
            event_to_receipts: HashMap::new(),
            prober: Prober::new(),
            neg_sessions: HashMap::new(),
            pending_backfills: BTreeSet::new(),
            pending_neg_credit: HashMap::new(),
            discovery_handle: None,
            discovery_authors: BTreeSet::new(),
            events_by_relay_kind: HashMap::new(),
            next_attempt_correlation: Some(0),
            attempt_correlations: HashMap::new(),
            admission: RelayAdmissionPolicy::default(),
            discovered_private_relays_rejected: 0,
            store_degraded: None,
            transport_degraded: None,
            #[cfg(test)]
            projection_store_queries: Cell::new(0),
            #[cfg(test)]
            router_compiles: Cell::new(0),
        }
    }

    /// Thread the operator's discovered-relay admission policy through
    /// construction (issue #121). Chained onto [`Self::new`] by the runtime
    /// (`engine_loop`); left at the secure default (reject every discovered
    /// private/loopback/onion host) everywhere else, so every test and every
    /// caller that does not opt local hosts in is fail-closed by default.
    #[must_use]
    pub fn with_relay_admission(mut self, admission: RelayAdmissionPolicy) -> Self {
        self.admission = admission;
        self
    }

    /// Record an ingest/read persistence failure (issue #122) without
    /// panicking: latch the first error message (read-only degrade) and push
    /// a fresh diagnostics snapshot so an observer sees the degraded state
    /// immediately. Idempotent — a later failure keeps the first message.
    fn degrade_store(&mut self, err: PersistenceError, effects: &mut Vec<Effect>) {
        if self.store_degraded.is_none() {
            self.store_degraded = Some(err.to_string());
        }
        effects.push(Effect::EmitDiagnostics(self.diagnostics_snapshot()));
    }

    /// Mint the next [`AttemptCorrelation`] (issue #93). Checked, typed
    /// exhaustion -- same discipline as [`Self::alloc_receipt_id`]'s
    /// `next_unaccepted_receipt` counter.
    fn alloc_attempt_correlation(
        &mut self,
    ) -> Result<AttemptCorrelation, AttemptCorrelationExhausted> {
        let id = self
            .next_attempt_correlation
            .ok_or(AttemptCorrelationExhausted)?;
        self.next_attempt_correlation = id.checked_add(1);
        Ok(AttemptCorrelation(id))
    }

    #[cfg(test)]
    fn set_next_attempt_correlation_for_test(&mut self, next: Option<u64>) {
        self.next_attempt_correlation = next;
    }

    /// The one, ever, resolution of `correlation`'s `HandoffResult` (issue
    /// #93). An unknown correlation (already resolved, or never minted by
    /// this process) is a structural no-op -- this is what makes a
    /// defensive duplicate delivery harmless even though the transport side
    /// never actually produces one.
    ///
    /// `Written` is the ONLY case that emits `WriteStatus::Sent` -- the
    /// synchronous "queue-accepted" `Sent` this reducer used to emit right
    /// after handing a frame to the pool is gone; a claim of `Sent` is now
    /// truthful only once transport has actually confirmed the write.
    /// `NotHandedOff`/`Ambiguous` stay a typed INTERNAL fact only: no
    /// receipt status, no retry action -- #96 wires governed visibility,
    /// #95 wires the scheduler that acts on it. This unit's whole job is
    /// making sure the fact reaches the engine at all, correlated and
    /// generation-safe, never silently dropped or requeued by transport.
    fn on_event_handoff(
        &mut self,
        correlation: AttemptCorrelation,
        result: HandoffResult,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(target) = self.attempt_correlations.remove(&correlation) else {
            return effects;
        };
        if let HandoffResult::Written = result {
            if let Some(pending) = self.pending.get(&target.receipt) {
                Self::notify(pending, WriteStatus::Sent(target.relay.clone()));
                effects.push(Effect::EmitReceipt(
                    target.receipt,
                    WriteStatus::Sent(target.relay.clone()),
                ));
            } else if !target.ephemeral_sinks.is_empty() {
                let status = WriteStatus::Sent(target.relay.clone());
                for sink in &target.ephemeral_sinks {
                    sink.on_status(status.clone());
                }
                effects.push(Effect::EmitReceipt(target.receipt, status));
            }
        }
        effects
    }

    /// Rebuild volatile ownership from the journal without reinserting a
    /// single row. Called exactly once by the runtime before its first
    /// command. No retry clock is armed here: #79 owns eligibility policy.
    pub fn recover_on_boot(&mut self) -> Vec<Effect> {
        let recovered = self.resolver.store().recover_outbox();
        let mut effects = Vec::new();
        for intent in recovered {
            let parsed_routing = Self::parse_routing_snapshot(&intent.routing);
            let routing_valid = parsed_routing.is_some();
            let routing = parsed_routing.unwrap_or_else(|| {
                WriteRouting::PrivateNarrow(PrivateRoute {
                    relays: NarrowOnly::new(Vec::<RelayUrl>::new()),
                })
            });
            let id = ReceiptId(intent.receipt_id);
            let durability = match intent.durability {
                WriteDurability::Durable => Durability::Durable,
                WriteDurability::AtMostOnce => Durability::AtMostOnce,
            };
            let attempts = self.resolver.store().recover_attempts(intent.intent_id);
            let already_signed = intent.sig_state == IntentSigState::Signed;
            self.pending.insert(
                id,
                PendingWrite {
                    durability,
                    routing,
                    routing_valid,
                    sinks: Vec::new(),
                    intent_id: Some(intent.intent_id),
                    signing_pubkey: intent.expected_pubkey,
                    frozen: intent.frozen.clone(),
                    already_signed,
                    sign_request_in_flight: false,
                    sign_generation: 0,
                    event_id: already_signed.then_some(intent.frozen.id),
                    pending_relays: BTreeSet::new(),
                    unstarted_relays: BTreeSet::new(),
                    route_blocked_relays: BTreeSet::new(),
                    attempt_ordinals: BTreeMap::new(),
                },
            );

            if !already_signed {
                continue;
            }

            let Ok(attempts) = attempts else {
                // Corrupt/unknown attempt evidence must not panic and must
                // not make the parent journal disappear. Keep the rebuilt
                // obligation owned, fail closed on wire output, and await an
                // explicit repair/migration.
                continue;
            };

            let Ok(revisions) = self
                .resolver
                .store()
                .recover_route_revisions(intent.intent_id)
            else {
                // As with corrupt attempt evidence, keep the parent intent
                // owned but emit no wire fact from undecodable persistence.
                continue;
            };
            let mut durable_relays = revisions
                .into_iter()
                .flat_map(|revision| revision.relays)
                .collect::<BTreeSet<_>>();
            let mut existing_relays = BTreeSet::new();
            for attempt in attempts {
                durable_relays.insert(attempt.relay.clone());
                existing_relays.insert(attempt.relay.clone());
                if attempt.outcome != AttemptOutcome::Started {
                    continue;
                }
                match durability {
                    Durability::Durable => {
                        let Ok(correlation) = self.alloc_attempt_correlation() else {
                            // No unique handoff identity means no volatile
                            // lane bookkeeping and no wire fact.
                            continue;
                        };
                        if let Some(pending) = self.pending.get_mut(&id) {
                            pending.pending_relays.insert(attempt.relay.clone());
                            pending
                                .attempt_ordinals
                                .insert(attempt.relay.clone(), attempt.ordinal);
                        }
                        self.event_to_receipts
                            .entry(attempt.event.id)
                            .or_default()
                            .insert(id);
                        self.attempt_correlations.insert(
                            correlation,
                            AttemptCorrelationTarget {
                                receipt: id,
                                relay: attempt.relay.clone(),
                                ephemeral_sinks: Vec::new(),
                            },
                        );
                        // Exact retained bytes, same ordinal. The Started row
                        // already predates this replay effect.
                        effects.push(Effect::PublishEvent(
                            attempt.relay,
                            attempt.event,
                            correlation,
                        ));
                    }
                    Durability::AtMostOnce => {
                        if self
                            .resolver
                            .store_mut()
                            .finish_attempt(
                                intent.intent_id,
                                &attempt.relay,
                                attempt.ordinal,
                                AttemptOutcome::OutcomeUnknown,
                            )
                            .is_err()
                        {
                            if let Some(pending) = self.pending.get_mut(&id) {
                                pending.pending_relays.insert(attempt.relay.clone());
                                pending
                                    .attempt_ordinals
                                    .insert(attempt.relay.clone(), attempt.ordinal);
                            }
                            self.event_to_receipts
                                .entry(attempt.event.id)
                                .or_default()
                                .insert(id);
                        }
                    }
                    Durability::Ephemeral => unreachable!(),
                }
            }

            // Every persisted route remains owned even if the dynamic
            // directory is now empty or removed it. Re-resolution may append
            // new lanes, but can never subtract durable ones.
            let mut lanes_to_start = durable_relays
                .difference(&existing_relays)
                .cloned()
                .collect::<BTreeSet<_>>();
            if routing_valid {
                let current_routes = self
                    .pending
                    .get(&id)
                    .and_then(|pending| {
                        self.resolve_routes(&pending.routing, &intent.frozen.pubkey.to_hex())
                            .ok()
                    })
                    .unwrap_or_default();
                let new_routes = current_routes
                    .difference(&durable_relays)
                    .cloned()
                    .collect::<BTreeSet<_>>();
                if !new_routes.is_empty() {
                    match self
                        .resolver
                        .store_mut()
                        .record_route_revision(intent.intent_id, current_routes)
                    {
                        Ok(revision) => {
                            debug_assert!(new_routes.is_subset(&revision.relays));
                            lanes_to_start.extend(new_routes);
                        }
                        Err(_) => {
                            if let Some(pending) = self.pending.get_mut(&id) {
                                pending.route_blocked_relays.extend(new_routes);
                            }
                        }
                    }
                }
            }

            for relay in lanes_to_start {
                let Ok(correlation) = self.alloc_attempt_correlation() else {
                    // Exhaustion is typed and fail-closed: do not create a
                    // new Started lane that cannot be correlated to transport.
                    continue;
                };
                let attempt = match self.resolver.store_mut().start_attempt(
                    intent.intent_id,
                    relay.clone(),
                    intent.frozen.clone(),
                ) {
                    Ok(attempt) => attempt,
                    Err(_) => {
                        if let Some(pending) = self.pending.get_mut(&id) {
                            pending.unstarted_relays.insert(relay);
                        }
                        continue;
                    }
                };
                if let Some(pending) = self.pending.get_mut(&id) {
                    pending.pending_relays.insert(relay.clone());
                    pending
                        .attempt_ordinals
                        .insert(relay.clone(), attempt.ordinal);
                }
                self.event_to_receipts
                    .entry(intent.frozen.id)
                    .or_default()
                    .insert(id);
                self.attempt_correlations.insert(
                    correlation,
                    AttemptCorrelationTarget {
                        receipt: id,
                        relay: relay.clone(),
                        ephemeral_sinks: Vec::new(),
                    },
                );
                effects.push(Effect::PublishEvent(
                    relay,
                    intent.frozen.clone(),
                    correlation,
                ));
            }
        }
        effects
    }

    /// Attach another observer to an existing durable receipt and replay
    /// its retained facts. Unknown ids do not create state.
    pub fn reattach_receipt(
        &mut self,
        id: ReceiptId,
        sink: Box<dyn ReceiptSink>,
    ) -> ReattachOutcome {
        let receipt = match self.resolver.store().reattach_receipt(id.0) {
            Ok(Some(receipt)) => receipt,
            Ok(None) => return ReattachOutcome::NotFound,
            Err(_) => return ReattachOutcome::RetainedButUnreadable,
        };
        if self
            .pending
            .get(&id)
            .is_some_and(|pending| !pending.routing_valid)
        {
            // Boot retained the obligation but could not interpret its
            // frozen routing policy. Replaying even the readable receipt
            // prefix would falsely imply that this observer is attached to
            // actionable live work, and registering it would leak later
            // signer facts from an obligation whose destination is unknown.
            return ReattachOutcome::RetainedButUnreadable;
        }
        let attempts = match receipt.intent_id {
            Some(intent_id) => {
                let attempts = match self.resolver.store().recover_attempts(intent_id) {
                    Ok(attempts) => attempts,
                    Err(_) => return ReattachOutcome::RetainedButUnreadable,
                };
                if self
                    .resolver
                    .store()
                    .recover_route_revisions(intent_id)
                    .is_err()
                {
                    return ReattachOutcome::RetainedButUnreadable;
                }
                attempts
            }
            None => Vec::new(),
        };
        let status = match receipt.state {
            ReceiptState::Accepted => WriteStatus::Accepted,
            ReceiptState::Signed => WriteStatus::Signed(receipt.frozen_id),
            ReceiptState::Compensated => WriteStatus::Failed("write compensated".to_string()),
            ReceiptState::Abandoned => {
                WriteStatus::Failed("ephemeral write abandoned after restart".to_string())
            }
        };
        sink.on_status(status);
        if receipt.state == ReceiptState::Accepted
            && self
                .pending
                .get(&id)
                .is_some_and(|pending| !pending.already_signed)
        {
            sink.on_status(WriteStatus::AwaitingCapability);
        }
        if receipt.intent_id.is_some() {
            for attempt in attempts {
                let status = match attempt.outcome {
                    // Started is only the crash-safe pre-wire fact. #93
                    // deliberately moved Sent to the later transport
                    // Written result, so replaying Started as Sent would
                    // recreate the exact false claim this seam removes.
                    AttemptOutcome::Started => continue,
                    AttemptOutcome::Acked => WriteStatus::Acked(attempt.relay),
                    AttemptOutcome::Rejected(reason) => {
                        WriteStatus::Rejected(attempt.relay, reason)
                    }
                    AttemptOutcome::GaveUp => WriteStatus::GaveUp(attempt.relay),
                    AttemptOutcome::OutcomeUnknown => WriteStatus::OutcomeUnknown(attempt.relay),
                };
                sink.on_status(status);
            }
        }
        if let Some(pending) = self.pending.get(&id) {
            for relay in &pending.unstarted_relays {
                sink.on_status(WriteStatus::PersistenceBlocked(relay.clone()));
            }
            for relay in &pending.route_blocked_relays {
                sink.on_status(WriteStatus::RoutePersistenceBlocked(relay.clone()));
            }
        }
        if let Some(pending) = self.pending.get_mut(&id) {
            pending.sinks.push(Rc::from(sink));
        }
        ReattachOutcome::Attached
    }

    /// Read-only access to the resolver's current demand (test/diagnostic
    /// convenience — the whole point of a headlessly-testable reducer is
    /// that its state can be inspected directly). Returns the TRUE
    /// `ContextualAtom` set (#118, fixed ahead of #107): #106 kept this
    /// surface `ConcreteFilter`-only, reconstructing context via a static
    /// default -- exact ONLY as long as nothing in production constructs a
    /// non-default `Demand`. #107's `SourceAuthority::Pinned` is the first
    /// production path that does, so a reconstruction would silently
    /// collapse two genuinely-distinct atoms (same selection, different
    /// context) that the resolver correctly tracks as two independent
    /// entries into one. Widened rather than patched with an assertion,
    /// per the repo's no-compat-alias convention -- this mirrors
    /// `nmp_resolver::Engine::active_demand()` exactly.
    pub fn active_demand(&self) -> BTreeSet<ContextualAtom> {
        self.resolver.active_demand()
    }

    /// Read-only coverage introspection (test/diagnostic convenience,
    /// mirroring `active_demand`): the proven interval for `atom`'s
    /// window-erased shape at `relay`, if any coverage has been recorded.
    /// `atom` is the atom's TRUE `ContextualAtom` (#118, fixed ahead of
    /// #107) -- the caller supplies the actual context an atom was
    /// acquired under, never a reconstruction. Before this fix, a
    /// `ConcreteFilter`-only signature reconstructed `source`/`access` via
    /// `Demand::from_filter`'s static default, which was exact only as
    /// long as every production atom took that default path; #107's
    /// `SourceAuthority::Pinned` breaks that assumption; the reconstruction
    /// would then compute the WRONG `CoverageKey` and silently report
    /// "not covered" for coverage that IS actually proven.
    pub fn get_coverage(
        &self,
        atom: &ContextualAtom,
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
        let mut snapshot = diagnostics::build(
            self.router.diagnostics(),
            self.router.plan(),
            &self.events_by_relay_kind,
            self.discovered_private_relays_rejected,
            |relay, key| self.resolver.store().get_coverage(key, relay),
        );
        // Surface the read-only degrade signal (issue #122) if an ingest/read
        // door has failed — the one persistence-health fact `build` cannot
        // see on its own.
        snapshot.store_degraded = self.store_degraded.clone();
        snapshot.transport_degraded = self.transport_degraded.clone();
        snapshot
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
        match self.resolver.store_mut().expire_due(now) {
            Ok(expired) if !expired.is_empty() => {
                let removed: Vec<_> = expired.into_iter().map(|se| se.event).collect();
                match self.resolver.retract(removed) {
                    Ok(_delta) => {
                        self.recompile(&mut effects);
                        self.refresh_all_handles(&mut effects);
                    }
                    Err(e) => self.degrade_store(e, &mut effects),
                }
            }
            Ok(_) => {}
            Err(e) => self.degrade_store(e, &mut effects),
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
            EngineMsg::RelayHealth(slot, health) => self.on_relay_health(slot, health),
            EngineMsg::RelayFrame(handle, frame) => self.on_relay_frame(handle, frame),
            EngineMsg::RelayFrames(frames) => self.on_relay_frames(frames),
            EngineMsg::SignerCompleted(id, generation, result) => {
                self.on_signer_completed(id, generation, result)
            }
            EngineMsg::SignerUnavailable(id, generation) => {
                self.on_signer_unavailable(id, generation)
            }
            EngineMsg::SignerAttached(pk) => self.on_signer_attached(pk),
            EngineMsg::CancelWrite(id) => self.on_cancel_write(id),
            EngineMsg::EventHandoff(correlation, result) => {
                self.on_event_handoff(correlation, result)
            }
            EngineMsg::Tick(now) => self.tick(now),
        }
    }

    fn on_relay_health(&mut self, slot: u32, health: RelayHealth) -> Vec<Effect> {
        self.transport_degraded = health.last_error.or_else(|| {
            (health.invalid_signature_count > 0).then(|| {
                format!(
                    "relay slot {slot} rejected {} invalid signature frame(s)",
                    health.invalid_signature_count
                )
            })
        });
        vec![Effect::EmitDiagnostics(self.diagnostics_snapshot())]
    }

    // ---- subscribe / unsubscribe / re-root ------------------------------

    fn on_subscribe(&mut self, query: LiveQuery, sink: Box<dyn RowSink>) -> Vec<Effect> {
        let mut effects = Vec::new();
        // Graph construction can read the store (a `Derived` binding resolves
        // its inner query). On a persistence failure (issue #122) degrade to
        // read-only and install NO handle rather than panic — the observer
        // simply receives no rows.
        let (qh, _delta) = match self.resolver.subscribe(query) {
            Ok(v) => v,
            Err(e) => {
                self.degrade_store(e, &mut effects);
                return effects;
            }
        };
        let id = qh.id();
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
                last_rows: BTreeMap::new(),
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
        self.active_pubkey = pk;
        let mut effects = Vec::new();
        // Re-rooting reactive nodes can re-query the store (a `Derived`
        // binding over a reactive field). Degrade to read-only on a
        // persistence failure (issue #122) rather than panic.
        if let Err(e) = self.resolver.set_active_pubkey(pk) {
            self.degrade_store(e, &mut effects);
            return effects;
        }
        self.recompile(&mut effects);
        self.refresh_all_handles(&mut effects);
        if let Some(pk) = pk {
            // The runtime moves its active signer pointer before delivering
            // this message. Re-arm matching accepted work here as well as
            // on SignerAttached so both ordering cases (activate→attach and
            // attach→activate) converge without polling.
            effects.extend(self.on_signer_attached(pk));
        }
        effects
    }

    // ---- write outbox (D: intent -> signed -> routed -> sent -> acked) --

    /// `Publish` (issues #2/#3 U3): enter durable/at-most-once writes through
    /// `resolver.accept_local` exactly once. The store allocates both ids
    /// and commits the canonical pending row, obligation and receipt before
    /// `Accepted` is observable. Ephemeral uses the distinct receipt-only
    /// door: no pending row and no retry obligation, but still a stable,
    /// reattachable receipt as required by the promoted VISION.
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
        let WriteIntent {
            payload,
            durability,
            routing,
        } = intent;

        let signing_pubkey = match &payload {
            WritePayload::Unsigned(unsigned) => match self.active_pubkey {
                Some(active) if active == unsigned.pubkey => active,
                Some(_) => {
                    return self.fail_unaccepted(
                        sink,
                        "unsigned draft author does not match current active account".to_string(),
                    );
                }
                None => {
                    return self.fail_unaccepted(
                        sink,
                        "unsigned publish requires an active account".to_string(),
                    );
                }
            },
            // Already-signed payloads are verified verbatim and never ask a
            // local signer, so their author is intrinsically frozen.
            WritePayload::Signed(event) => event.pubkey,
        };

        if let WritePayload::Signed(event) = &payload {
            if let Err(err) = event.verify() {
                return self.fail_unaccepted(sink, err.to_string());
            }
        }

        let frozen = match Self::freeze_payload(&payload) {
            Ok(frozen) => frozen,
            Err(reason) => return self.fail_unaccepted(sink, reason),
        };

        let (id, intent_id, already_signed, accepted_signed_event) = if durability
            == Durability::Ephemeral
        {
            match self
                .resolver
                .store_mut()
                .accept_ephemeral(frozen.id, signing_pubkey)
            {
                Ok(receipt_id) => (ReceiptId(receipt_id), None, false, None),
                Err(err) => return self.fail_unaccepted(sink, err.to_string()),
            }
        } else {
            let store_durability = match durability {
                Durability::Durable => WriteDurability::Durable,
                Durability::AtMostOnce => WriteDurability::AtMostOnce,
                Durability::Ephemeral => unreachable!("handled above"),
            };
            let accept = AcceptWrite {
                frozen: frozen.clone(),
                expected_pubkey: signing_pubkey,
                signing_identity_ref: signing_pubkey.to_hex(),
                durability: store_durability,
                routing: Self::routing_snapshot(&routing),
                // Treat an unsigned acceptance as reattachable signer work.
                // If a signer is already present the immediate request below
                // promotes it; if not, restart safely re-requests it.
                sig_state: match payload {
                    WritePayload::Unsigned(_) => IntentSigState::AwaitingSigner,
                    WritePayload::Signed(_) => IntentSigState::Pending,
                },
                accepted_at: self.clock,
            };
            let (outcome, _delta) = match self.resolver.accept_local(accept) {
                Ok(value) => value,
                Err(err) => return self.fail_unaccepted(sink, err.to_string()),
            };
            let Some(intent_id) = outcome.journaled_intent_id() else {
                let AcceptOutcome::Refused(reason) = outcome else {
                    unreachable!("only Refused omits journal ids")
                };
                return self.fail_unaccepted(sink, format!("write refused: {reason:?}"));
            };
            let receipt_id = outcome
                .journaled_receipt_id()
                .expect("journaled intent always has a receipt id");
            let accepted_signed_event = match &outcome {
                AcceptOutcome::Duplicate { row, .. } if row.event.sig != sentinel_signature() => {
                    Some(row.event.clone())
                }
                _ => None,
            };
            (
                ReceiptId(receipt_id),
                Some(intent_id),
                accepted_signed_event.is_some(),
                accepted_signed_event,
            )
        };

        let mut effects = Vec::new();
        sink.on_status(WriteStatus::Accepted);
        effects.push(Effect::EmitReceipt(id, WriteStatus::Accepted));

        self.pending.insert(
            id,
            PendingWrite {
                durability,
                routing,
                routing_valid: true,
                sinks: vec![Rc::from(sink)],
                intent_id,
                signing_pubkey,
                frozen: frozen.clone(),
                already_signed,
                sign_request_in_flight: false,
                sign_generation: 0,
                event_id: None,
                pending_relays: BTreeSet::new(),
                unstarted_relays: BTreeSet::new(),
                route_blocked_relays: BTreeSet::new(),
                attempt_ordinals: BTreeMap::new(),
            },
        );

        if durability != Durability::Ephemeral {
            // The pending row was committed before Accepted. Expose it only
            // through ordinary demand recompilation/query refresh.
            self.recompile(&mut effects);
            self.refresh_all_handles(&mut effects);
        }

        match payload {
            WritePayload::Unsigned(unsigned) => {
                if already_signed {
                    self.on_signed(
                        id,
                        accepted_signed_event
                            .expect("already-signed acceptance carries its canonical event"),
                        &mut effects,
                    );
                } else {
                    if let Some(pending) = self.pending.get_mut(&id) {
                        pending.sign_request_in_flight = true;
                        pending.sign_generation += 1;
                        let generation = pending.sign_generation;
                        effects.push(Effect::RequestSign(id, generation, unsigned));
                    }
                }
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
        generation: u64,
        result: Result<SignedEvent, SignerError>,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(pending) = self.pending.get_mut(&id) else {
            return effects;
        };
        if !pending.sign_request_in_flight || pending.sign_generation != generation {
            return effects;
        }
        pending.sign_request_in_flight = false;
        match result {
            Ok(event) => self.on_signed(id, event, &mut effects),
            Err(err) => {
                self.fail_and_compensate(id, err.to_string(), &mut effects);
            }
        }
        effects
    }

    fn on_signer_unavailable(&mut self, id: ReceiptId, generation: u64) -> Vec<Effect> {
        let mut effects = Vec::new();
        if let Some(pending) = self.pending.get_mut(&id) {
            if !pending.sign_request_in_flight || pending.sign_generation != generation {
                return effects;
            }
            pending.sign_request_in_flight = false;
            Self::notify(pending, WriteStatus::AwaitingCapability);
            effects.push(Effect::EmitReceipt(id, WriteStatus::AwaitingCapability));
        }
        effects
    }

    fn on_signer_attached(&mut self, pk: PublicKey) -> Vec<Effect> {
        let mut effects = Vec::new();
        for (id, pending) in &mut self.pending {
            if pending.signing_pubkey == pk
                && pending.event_id.is_none()
                && !pending.already_signed
                && !pending.sign_request_in_flight
            {
                pending.sign_request_in_flight = true;
                pending.sign_generation += 1;
                effects.push(Effect::RequestSign(
                    *id,
                    pending.sign_generation,
                    UnsignedEvent {
                        id: Some(pending.frozen.id),
                        pubkey: pending.frozen.pubkey,
                        created_at: pending.frozen.created_at,
                        kind: pending.frozen.kind,
                        tags: pending.frozen.tags.clone(),
                        content: pending.frozen.content.clone(),
                    },
                ));
            }
        }
        effects
    }

    fn on_cancel_write(&mut self, id: ReceiptId) -> Vec<Effect> {
        let mut effects = Vec::new();
        self.fail_and_compensate(
            id,
            "write cancelled before signing".to_string(),
            &mut effects,
        );
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
        let Some(pending) = self.pending.get(&id) else {
            return; // unknown/already-resolved receipt id.
        };
        if pending.event_id.is_some() {
            return; // duplicate/delayed signer completion after routing.
        }

        if let Err(reason) = Self::validate_signed_template(&pending.frozen, &event) {
            self.fail_and_compensate(id, reason, effects);
            return;
        }

        let mut co_receipts = Vec::new();
        if let Some(intent_id) = pending.intent_id {
            if !pending.already_signed {
                match self
                    .resolver
                    .store_mut()
                    .promote_signed(intent_id, event.sig)
                {
                    Ok(PromoteOutcome::Promoted { co_signed, .. }) => {
                        // The store atomically promotes every exact-duplicate
                        // co-owner against the same canonical bytes. Advance
                        // each matching in-memory obligation too; otherwise
                        // an offline co-owner could remain stranded forever
                        // behind a row that is already validly signed.
                        for co_intent in co_signed {
                            if let Some((receipt_id, co_pending)) = self
                                .pending
                                .iter_mut()
                                .find(|(_, candidate)| candidate.intent_id == Some(co_intent))
                            {
                                co_pending.already_signed = true;
                                co_receipts.push(*receipt_id);
                            }
                        }
                    }
                    Ok(PromoteOutcome::NotFound) => {
                        self.fail_and_compensate(
                            id,
                            "accepted intent was unavailable for signature promotion".to_string(),
                            effects,
                        );
                        return;
                    }
                    Err(err) => {
                        self.fail_and_compensate(id, err.to_string(), effects);
                        return;
                    }
                }
            }
        }

        for co_receipt in co_receipts {
            self.on_signed(co_receipt, event.clone(), effects);
        }

        if let Some(pending) = self.pending.get_mut(&id) {
            pending.event_id = Some(event.id);
        }

        if let Some(pending) = self.pending.get(&id) {
            Self::notify(pending, WriteStatus::Signed(event.id));
            effects.push(Effect::EmitReceipt(id, WriteStatus::Signed(event.id)));
            if !pending.routing_valid {
                // Corrupt/unknown persisted routing is fail-closed, but the
                // durable obligation remains owned and reattachable. Never
                // silently `continue` it out of recovery and never guess a
                // relay. A future migration/explicit cancellation can
                // resolve it.
                return;
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
                    Self::notify(&pending, status.clone());
                    effects.push(Effect::EmitReceipt(id, status));
                }
                return;
            }
        };

        if let Some(pending) = self.pending.get(&id) {
            Self::notify(pending, WriteStatus::Routed(relays.clone()));
            effects.push(Effect::EmitReceipt(id, WriteStatus::Routed(relays.clone())));
        }

        // Dynamic routing policy is not itself an exact relay obligation.
        // Persist the resolved set before any corresponding attempt or wire
        // effect, so a failed attempt start remains discoverable on restart
        // even if the directory is then empty or changed.
        if let Some(intent_id) = self.pending.get(&id).and_then(|pending| pending.intent_id) {
            if self
                .resolver
                .store_mut()
                .record_route_revision(intent_id, relays.clone())
                .is_err()
            {
                if let Some(pending) = self.pending.get_mut(&id) {
                    pending.route_blocked_relays = relays.clone();
                }
                if let Some(pending) = self.pending.get(&id) {
                    for relay in &relays {
                        let status = WriteStatus::RoutePersistenceBlocked(relay.clone());
                        Self::notify(pending, status.clone());
                        effects.push(Effect::EmitReceipt(id, status));
                    }
                }
                return;
            }
        }

        // The durable attempt fact is committed before the corresponding
        // wire effect can exist. Ephemeral has no obligation/attempt row.
        let mut sent_relays = BTreeSet::new();
        let mut blocked_relays = BTreeSet::new();
        for relay in &relays {
            let Ok(correlation) = self.alloc_attempt_correlation() else {
                // Leave the lane and pending bookkeeping untouched. A
                // transport EVENT without a unique correlation cannot exist.
                continue;
            };
            let ordinal = match self.pending.get(&id).and_then(|p| p.intent_id) {
                Some(intent_id) => match self.resolver.store_mut().start_attempt(
                    intent_id,
                    relay.clone(),
                    event.clone(),
                ) {
                    Ok(attempt) => Some(attempt.ordinal),
                    Err(_) => {
                        blocked_relays.insert(relay.clone());
                        None
                    }
                },
                None => Some(0),
            };
            let Some(ordinal) = ordinal else {
                continue;
            };
            if let Some(pending) = self.pending.get_mut(&id) {
                if ordinal != 0 {
                    pending.attempt_ordinals.insert(relay.clone(), ordinal);
                }
            }
            sent_relays.insert(relay.clone());
            let ephemeral_sinks = self
                .pending
                .get(&id)
                .filter(|pending| pending.durability == Durability::Ephemeral)
                .map(|pending| pending.sinks.clone())
                .unwrap_or_default();
            self.attempt_correlations.insert(
                correlation,
                AttemptCorrelationTarget {
                    receipt: id,
                    relay: relay.clone(),
                    ephemeral_sinks,
                },
            );
            effects.push(Effect::PublishEvent(
                relay.clone(),
                event.clone(),
                correlation,
            ));
        }
        // `Sent` is no longer emitted synchronously here (issue #93): the
        // frame being handed to the pool's outbound queue is not the same
        // fact as the relay actually receiving it. `Self::on_event_handoff`
        // is the ONLY place `Sent` now fires, gated on the async `Written`
        // result. `sent_relays` above still drives `pending.pending_relays`
        // (below) and the correlation bookkeeping -- it just no longer
        // implies delivery on its own.
        if let Some(pending) = self.pending.get(&id) {
            for relay in &blocked_relays {
                let status = WriteStatus::PersistenceBlocked(relay.clone());
                Self::notify(pending, status.clone());
                effects.push(Effect::EmitReceipt(id, status));
            }
        }

        let ephemeral = matches!(
            self.pending.get(&id).map(|p| p.durability),
            Some(Durability::Ephemeral)
        );
        if ephemeral {
            // Fire-and-forget owns no ACK obligation. Each correlation has
            // snapshotted the observers needed for its async handoff result.
            self.pending.remove(&id);
        } else if let Some(pending) = self.pending.get_mut(&id) {
            pending.pending_relays = sent_relays;
            pending.unstarted_relays = blocked_relays;
            self.event_to_receipts
                .entry(event.id)
                .or_default()
                .insert(id);
        }
    }

    fn freeze_payload(payload: &WritePayload) -> Result<SignedEvent, String> {
        match payload {
            WritePayload::Unsigned(unsigned) => {
                let computed = EventId::new(
                    &unsigned.pubkey,
                    &unsigned.created_at,
                    &unsigned.kind,
                    &unsigned.tags,
                    &unsigned.content,
                );
                if let Some(declared) = unsigned.id {
                    if declared != computed {
                        return Err(
                            "unsigned event carries an id that does not match its body".into()
                        );
                    }
                }
                Ok(SignedEvent::new(
                    computed,
                    unsigned.pubkey,
                    unsigned.created_at,
                    unsigned.kind,
                    unsigned.tags.clone(),
                    unsigned.content.clone(),
                    sentinel_signature(),
                ))
            }
            WritePayload::Signed(event) => Ok(SignedEvent::new(
                event.id,
                event.pubkey,
                event.created_at,
                event.kind,
                event.tags.clone(),
                event.content.clone(),
                sentinel_signature(),
            )),
        }
    }

    fn validate_signed_template(frozen: &SignedEvent, signed: &SignedEvent) -> Result<(), String> {
        if signed.id != frozen.id
            || signed.pubkey != frozen.pubkey
            || signed.created_at != frozen.created_at
            || signed.kind != frozen.kind
            || signed.tags != frozen.tags
            || signed.content != frozen.content
        {
            return Err(
                "signer returned an event that does not match the accepted template".into(),
            );
        }
        signed
            .verify()
            .map_err(|err| format!("signer returned an invalid signature: {err}"))
    }

    fn routing_snapshot(routing: &WriteRouting) -> String {
        match routing {
            WriteRouting::AuthorOutbox => "author-outbox".to_string(),
            WriteRouting::ToInboxes(recipients) => format!(
                "to-inboxes:{}",
                recipients
                    .iter()
                    .map(PublicKey::to_hex)
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            WriteRouting::PrivateNarrow(route) => format!(
                "private-narrow-hex:{}",
                route
                    .relays
                    .iter()
                    .map(|relay| hex::encode(relay.to_string()))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            WriteRouting::PinnedHost(auth) => {
                format!("pinned-host-hex:{}", hex::encode(auth.host().to_string()))
            }
        }
    }

    fn parse_routing_snapshot(snapshot: &str) -> Option<WriteRouting> {
        if snapshot == "author-outbox" {
            return Some(WriteRouting::AuthorOutbox);
        }
        if let Some(keys) = snapshot.strip_prefix("to-inboxes:") {
            let recipients = if keys.is_empty() {
                Vec::new()
            } else {
                keys.split(',')
                    .map(PublicKey::from_hex)
                    .collect::<Result<Vec<_>, _>>()
                    .ok()?
            };
            return Some(WriteRouting::ToInboxes(recipients));
        }
        if let Some(encoded) = snapshot.strip_prefix("private-narrow-hex:") {
            let relays = if encoded.is_empty() {
                Vec::new()
            } else {
                encoded
                    .split(',')
                    .map(|part| {
                        let bytes = hex::decode(part).ok()?;
                        let url = String::from_utf8(bytes).ok()?;
                        RelayUrl::parse(&url).ok()
                    })
                    .collect::<Option<Vec<_>>>()?
            };
            return Some(WriteRouting::PrivateNarrow(PrivateRoute {
                relays: NarrowOnly::new(relays),
            }));
        }
        if let Some(encoded) = snapshot.strip_prefix("pinned-host-hex:") {
            let bytes = hex::decode(encoded).ok()?;
            let url = String::from_utf8(bytes).ok()?;
            let host = RelayUrl::parse(&url).ok()?;
            return Some(WriteRouting::PinnedHost(HostAuthority::from_selected_host(
                host,
            )));
        }
        None
    }

    fn fail_unaccepted(&mut self, sink: Box<dyn ReceiptSink>, reason: String) -> Vec<Effect> {
        // No store id exists on refusal/persistence failure by contract.
        // This correlation id is stream-local only and never enters the
        // durable receipt namespace.
        let id = match self.alloc_receipt_id() {
            Ok(id) => id,
            Err(err) => return vec![Effect::PublishFailed(err)],
        };
        let status = WriteStatus::Failed(reason);
        sink.on_status(status.clone());
        vec![Effect::EmitReceipt(id, status)]
    }

    fn fail_and_compensate(&mut self, id: ReceiptId, reason: String, effects: &mut Vec<Effect>) {
        let Some(pending) = self.pending.remove(&id) else {
            return;
        };

        if let Some(intent_id) = pending.intent_id {
            match self.resolver.store_mut().compensate_write(intent_id) {
                Ok(outcome @ CompensateOutcome::Compensated { .. }) => {
                    // The store compensation already committed; reacting only
                    // re-reads to recompute the graph. A read failure here
                    // (issue #122) degrades to read-only rather than panics.
                    match self
                        .resolver
                        .react_to_compensation(pending.frozen.clone(), &outcome)
                    {
                        Ok(_delta) => {
                            self.recompile(effects);
                            self.refresh_all_handles(effects);
                        }
                        Err(e) => self.degrade_store(e, effects),
                    }
                }
                Ok(CompensateOutcome::NotFound) => {
                    // Promotion already made the row valid. Never retract a
                    // signed row; cancellation/signing errors arriving late
                    // cannot rewrite cache truth.
                    self.pending.insert(id, pending);
                    return;
                }
                Err(err) => {
                    // Compensation itself failed atomically. Keep the
                    // in-memory obligation so the caller can retry rather
                    // than losing ownership of a still-visible pending row.
                    // Crucially, do NOT emit terminal Failed: persistence
                    // did not commit the terminal transition, so claiming it
                    // did would contradict both the row and journal. U4 owns
                    // durable retry scheduling; a later explicit cancel or
                    // signer completion can re-enter this door.
                    self.pending.insert(id, pending);
                    let _persistence_error = err;
                    return;
                }
            }
        }

        Self::notify(&pending, WriteStatus::Failed(reason.clone()));
        effects.push(Effect::EmitReceipt(id, WriteStatus::Failed(reason)));
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
    ///
    /// `PinnedHost` (#115) also never consults the directory — like
    /// `PrivateNarrow`, its one relay is exactly whatever the caller
    /// asserted via `HostAuthority::from_selected_host`. Unlike
    /// `PrivateNarrow`, an empty/unroutable state is structurally
    /// unreachable (`HostAuthority` always carries exactly one well-formed
    /// `RelayUrl`), so this arm is infallible where `PrivateNarrow`'s is
    /// not.
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
            WriteRouting::PinnedHost(auth) => Ok(BTreeSet::from([auth.host()])),
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
        let Some(ids) = self.event_to_receipts.get(&event_id).cloned() else {
            return;
        };
        let mut terminal = Vec::new();
        for id in ids {
            let Some(pending) = self.pending.get(&id) else {
                terminal.push(id);
                continue;
            };
            if !pending.pending_relays.contains(relay) {
                continue;
            }
            let attempt = pending
                .intent_id
                .zip(pending.attempt_ordinals.get(relay).copied());
            if let Some((intent_id, ordinal)) = attempt {
                let outcome = if status {
                    AttemptOutcome::Acked
                } else {
                    AttemptOutcome::Rejected(message.clone())
                };
                if self
                    .resolver
                    .store_mut()
                    .finish_attempt(intent_id, relay, ordinal, outcome)
                    .is_err()
                {
                    continue;
                }
            }
            let pending = self
                .pending
                .get_mut(&id)
                .expect("lane checked immediately above");
            pending.pending_relays.remove(relay);
            pending.attempt_ordinals.remove(relay);
            let new_status = if status {
                WriteStatus::Acked(relay.clone())
            } else {
                WriteStatus::Rejected(relay.clone(), message.clone())
            };
            Self::notify(pending, new_status.clone());
            effects.push(Effect::EmitReceipt(id, new_status));
            if pending.pending_relays.is_empty()
                && pending.unstarted_relays.is_empty()
                && pending.route_blocked_relays.is_empty()
            {
                terminal.push(id);
            }
        }
        for id in terminal {
            self.pending.remove(&id);
            if let Some(owners) = self.event_to_receipts.get_mut(&event_id) {
                owners.remove(&id);
            }
        }
        if self
            .event_to_receipts
            .get(&event_id)
            .is_some_and(BTreeSet::is_empty)
        {
            self.event_to_receipts.remove(&event_id);
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
            let attempt = self.pending.get(&id).and_then(|pending| {
                pending
                    .intent_id
                    .zip(pending.attempt_ordinals.get(relay).copied())
            });
            if let Some((intent_id, ordinal)) = attempt {
                if self
                    .resolver
                    .store_mut()
                    .finish_attempt(intent_id, relay, ordinal, AttemptOutcome::GaveUp)
                    .is_err()
                {
                    continue;
                }
            }
            let event_id = if let Some(pending) = self.pending.get_mut(&id) {
                pending.pending_relays.remove(relay);
                pending.attempt_ordinals.remove(relay);
                let status = WriteStatus::GaveUp(relay.clone());
                Self::notify(pending, status.clone());
                effects.push(Effect::EmitReceipt(id, status));
                if pending.pending_relays.is_empty()
                    && pending.unstarted_relays.is_empty()
                    && pending.route_blocked_relays.is_empty()
                {
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
                if let Some(owners) = self.event_to_receipts.get_mut(&event_id) {
                    owners.remove(&id);
                    if owners.is_empty() {
                        self.event_to_receipts.remove(&event_id);
                    }
                }
            }
        }
    }

    fn alloc_receipt_id(&mut self) -> Result<ReceiptId, PublishError> {
        const FIRST_UNACCEPTED_ID: u64 = 1u64 << 63;
        let current = self
            .next_unaccepted_receipt
            .ok_or(PublishError::ReceiptCorrelationIdExhausted)?;
        debug_assert!(current >= FIRST_UNACCEPTED_ID);
        self.next_unaccepted_receipt = (current > FIRST_UNACCEPTED_ID).then_some(current - 1);
        Ok(ReceiptId(current))
    }

    #[cfg(test)]
    fn set_next_unaccepted_receipt_for_test(&mut self, next: Option<u64>) {
        assert!(next.is_none_or(|id| id >= (1u64 << 63)));
        self.next_unaccepted_receipt = next;
    }

    fn notify(pending: &PendingWrite, status: WriteStatus) {
        for sink in &pending.sinks {
            sink.on_status(status.clone());
        }
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

    fn ingest_relay_events(
        &mut self,
        events: Vec<(SignedEvent, RelayObserved)>,
        effects: &mut Vec<Effect>,
    ) {
        if events.is_empty() {
            return;
        }
        let relay_list_authors: Vec<_> = events
            .iter()
            .filter_map(|(event, _)| (event.kind == nostr::Kind::RelayList).then_some(event.pubkey))
            .collect();
        for (event, observed) in &events {
            *self
                .events_by_relay_kind
                .entry(observed.relay.clone())
                .or_default()
                .entry(event.kind.as_u16())
                .or_insert(0) += 1;
        }
        match self.resolver.ingest_observed_detailed(events) {
            Err(error) => self.degrade_store(error, effects),
            Ok(ingest) => {
                let demand_changed = !ingest.delta.is_empty();
                let affected_handles = ingest.affected_handles;
                let satisfied_pending = !ingest.satisfied_intents.is_empty();
                for (intent_id, canonical) in ingest.satisfied_intents {
                    if let Some((receipt_id, pending)) = self
                        .pending
                        .iter_mut()
                        .find(|(_, pending)| pending.intent_id == Some(intent_id))
                    {
                        pending.already_signed = true;
                        pending.sign_request_in_flight = false;
                        let receipt_id = *receipt_id;
                        self.on_signed(receipt_id, canonical, effects);
                    }
                }
                let mut directory_changed = false;
                for author in relay_list_authors {
                    directory_changed |= self.ingest_relay_list_winner(author, effects);
                }

                // Ordinary committed rows do not change the active demand or
                // router plan. Avoid rebuilding it on every EVENT batch; a
                // resolver atom delta or an actual NIP-65 directory change is
                // the evidence that routing may differ.
                if demand_changed || directory_changed {
                    self.recompile(effects);
                } else {
                    // Event counters are diagnostics facts even when the
                    // demand/router plan is unchanged. Preserve the prior
                    // observable update without paying a full router compile.
                    effects.push(Effect::EmitDiagnostics(self.diagnostics_snapshot()));
                }

                // A demand/directory change may alter the capped source plan
                // and therefore evidence for otherwise-unrelated handles;
                // keep that path broad. The dominant ordinary-ingest path is
                // exact: refresh only subscriptions whose root filter matches
                // a changed row (or whose shared projection shape changed).
                if demand_changed || directory_changed || satisfied_pending {
                    self.refresh_all_handles(effects);
                } else {
                    self.refresh_handles(affected_handles, effects);
                }
            }
        }
    }

    fn on_relay_frames(&mut self, frames: Vec<(TransportRelayHandle, RelayFrame)>) -> Vec<Effect> {
        let mut effects = Vec::new();
        let mut events = Vec::new();
        for (handle, frame) in frames {
            match frame.into_event() {
                Ok(event) => {
                    let Some(relay) = self.slot_to_url.get(&handle.slot).cloned() else {
                        self.ingest_relay_events(std::mem::take(&mut events), &mut effects);
                        continue;
                    };
                    events.push((event, RelayObserved::new(relay, self.clock)));
                }
                Err(frame) => {
                    self.ingest_relay_events(std::mem::take(&mut events), &mut effects);
                    effects.extend(self.on_relay_frame(handle, frame));
                }
            }
        }
        self.ingest_relay_events(events, &mut effects);
        effects
    }

    fn on_relay_frame(&mut self, handle: TransportRelayHandle, frame: RelayFrame) -> Vec<Effect> {
        let mut effects = Vec::new();
        let msg = frame.into_message();
        let Some(relay) = self.slot_to_url.get(&handle.slot).cloned() else {
            return effects; // frame from a slot we never saw RelayConnected for.
        };

        match msg {
            RelayMessage::Event { event, .. } => {
                let event = event.into_owned();
                let observed = RelayObserved::new(relay, self.clock);
                self.ingest_relay_events(vec![(event, observed)], &mut effects);
            }
            RelayMessage::EndOfStoredEvents(sub_id) => {
                let wire_id = sub_id.as_str();
                let attributed = self.attribution.attribute_eose(&relay, wire_id, self.clock);
                for (key, interval) in attributed {
                    if let Some(atom) = self.attribution.shape_of(key) {
                        if let Err(e) = self
                            .resolver
                            .store_mut()
                            .record_coverage(&atom, &relay, interval)
                        {
                            // Persisting a coverage watermark failed (issue
                            // #122): degrade rather than panic. The
                            // in-memory `Effect::RecordCoverage` is skipped
                            // too — no watermark is claimed that did not
                            // durably land.
                            self.degrade_store(e, &mut effects);
                            continue;
                        }
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
        #[cfg(test)]
        self.router_compiles
            .set(self.router_compiles.get().saturating_add(1));
        self.sync_discovery(effects);
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
    fn sync_discovery(&mut self, effects: &mut Vec<Effect>) {
        let needed: BTreeSet<PubkeyHex> = self
            .resolver
            .active_demand()
            .into_iter()
            .filter_map(|atom| atom.filter.authors)
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
        let query = LiveQuery::from_filter(Filter {
            kinds: Some(BTreeSet::from([NIP65_RELAY_LIST_KIND])),
            authors: Some(Binding::Literal(self.discovery_authors.clone())),
            ..Filter::default()
        });
        // Building the internal discovery subscription can read the store.
        // On a persistence failure (issue #122) degrade to read-only and
        // open no discovery sub rather than panic.
        match self.resolver.subscribe(query) {
            Ok((handle, _delta)) => self.discovery_handle = Some(handle),
            Err(e) => self.degrade_store(e, effects),
        }
    }

    /// After ingesting a possible kind:10002 event for `author`, re-read the
    /// store's CURRENT winning relay-list event for them -- never trust the
    /// just-arrived frame directly. `EventStore::query` only ever returns
    /// the current replaceable-event winner (`nmp-store`'s own contract), so
    /// this is correct regardless of cross-relay arrival order: a stale/
    /// older copy that already lost the replaceable race at `insert` time
    /// can never overwrite the directory with worse data than what the
    /// store itself considers authoritative.
    fn ingest_relay_list_winner(
        &mut self,
        author: nostr::PublicKey,
        effects: &mut Vec<Effect>,
    ) -> bool {
        let filter = ConcreteFilter {
            kinds: Some(BTreeSet::from([NIP65_RELAY_LIST_KIND])),
            authors: Some(BTreeSet::from([author.to_hex()])),
            ..ConcreteFilter::default()
        };
        // Re-reading the store's current relay-list winner can fail on I/O
        // (issue #122): degrade to read-only rather than panic. The
        // directory simply isn't updated for this author on this frame.
        let winner = match self.resolver.store().query(&filter.to_nostr()) {
            Ok(rows) => rows.into_iter().next(),
            Err(e) => {
                self.degrade_store(e, effects);
                return false;
            }
        };
        let Some(winner) = winner else {
            return false;
        };
        // Relay admission (issue #121): these relays are DISCOVERED — parsed
        // straight off a network-sourced (validly-signed, but untrusted-
        // content) kind:10002. Gate them on host classification + the
        // operator's opt-in local allowlist BEFORE they become routable
        // `Nip65Write`/`Nip65Read` lanes. A rejected relay never enters the
        // directory, so it never becomes a router candidate and never reaches
        // `pool.ensure_open` — the SSRF / forced-Tor path is closed
        // structurally, not filtered downstream.
        //
        // FORWARD GUARD: this is currently the SOLE network-discovery path
        // into the relay directory. ANY future network-sourced relay ingest —
        // a kind:10050 DM-inbox list, nprofile/nevent relay hints, a
        // provenance "seen here" lane, etc. — MUST route its parsed relays
        // through `self.admission.filter_discovered(..)` before calling
        // `directory.ingest_*`, or the structural exclusion proven here is
        // silently lost for that new source. Discovery is untrusted;
        // operator config (the `LiveDirectory` builder lanes) is not and is
        // deliberately NOT gated here.
        let (write_relays, write_rejected) = self
            .admission
            .filter_discovered(parse_nip65_write_relays(&winner.event));
        let (read_relays, read_rejected) = self
            .admission
            .filter_discovered(parse_nip65_read_relays(&winner.event));
        self.discovered_private_relays_rejected = self
            .discovered_private_relays_rejected
            .saturating_add(write_rejected + read_rejected);
        let author = author.to_hex();
        let before_known = self.directory.knows_write_relays(&author);
        let before_write = self.directory.write_relays(&author);
        let before_read = self.directory.read_relays(&author);
        self.directory
            .ingest_write_relays(author.clone(), write_relays);
        self.directory
            .ingest_read_relays(author.clone(), read_relays);
        before_known != self.directory.knows_write_relays(&author)
            || before_write != self.directory.write_relays(&author)
            || before_read != self.directory.read_relays(&author)
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
        // Seeding the reconciler reads the local store's holdings for this
        // shape. On an I/O failure (issue #122) degrade to read-only and do
        // not open the session rather than panic — the `Close` pushed above
        // still stands, so the sub-id is simply released.
        let local_rows = match self.resolver.store().query(&neg_filter.to_nostr()) {
            Ok(rows) => rows,
            Err(e) => {
                self.degrade_store(e, effects);
                return;
            }
        };
        let local_ids: Vec<(u64, EventId)> = local_rows
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
            // An id-targeted one-shot backfill fetch, not itself tied to
            // any live Demand (#106): no `authors` binding at all, so
            // `Public`/`Public` is the exact context `Demand::from_filter`'s
            // static default would assign an authorless filter -- and this
            // sub carries no coverage credit of its own anyway (`absorbed`
            // is empty below; the credit it unlocks is `sub_id`'s, via
            // `pending_neg_credit`).
            let backfill_sub = SubId::for_wire(
                relay.clone(),
                &backfill,
                &SourceAuthority::Public,
                AccessContext::Public,
            );
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
                if let Err(e) = self
                    .resolver
                    .store_mut()
                    .record_coverage(&shape, relay, interval)
                {
                    // Coverage-watermark persistence failed (issue #122):
                    // degrade to read-only, claim no watermark that did not
                    // land, and do not panic.
                    self.degrade_store(e, effects);
                    continue;
                }
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
        self.refresh_handles(ids, effects);
    }

    fn refresh_handles(
        &mut self,
        ids: impl IntoIterator<Item = HandleId>,
        effects: &mut Vec<Effect>,
    ) {
        for id in ids {
            // The resolver also owns internal handles (notably the
            // self-bootstrap discovery query). They participate in graph
            // invalidation but have no app projection state here. Reject
            // them before `refresh_handle` opens any store read.
            if self.handles.contains_key(&id) {
                self.refresh_handle(id, effects);
            }
        }
    }

    /// Recompute `id`'s current row set + acquisition evidence; emit (and
    /// synchronously deliver to its sink) `Effect::EmitRows` only if either
    /// changed since the last refresh -- and, when something DID change, the
    /// row payload is ALWAYS just the incremental added/sources-grew/removed
    /// delta against `state.last_rows`, never the full current set (see
    /// `RowDelta`'s doc: this is what keeps a long-running subscription's
    /// total delivered row volume ~O(distinct rows) instead of O(rows²)).
    /// Evidence can change with no row change at all (a watermark advancing,
    /// or a source's link status flipping) -- that case still emits,
    /// carrying an EMPTY row delta alongside the new evidence. #105:
    /// per-id provenance growth is detected the SAME way -- a plain value
    /// compare of `state.last_rows`'s remembered source set against this
    /// recompute's -- so a lifecycle-driven recompute of some OTHER
    /// handle's query (`refresh_all_handles`, e.g. on ANY subscribe/
    /// unsubscribe) can never spuriously emit a `SourcesGrew` for a row
    /// whose provenance did not actually change.
    fn refresh_handle(&mut self, id: HandleId, effects: &mut Vec<Effect>) {
        // A read failure while snapshotting this handle's rows (issue #122)
        // degrades to read-only: leave the handle's LAST delivered rows
        // untouched (never fabricate a phantom retraction from a failed
        // read) and surface the degrade on diagnostics instead of panicking.
        let (current, evidence) = match self.rows_and_evidence_for(id) {
            Ok(v) => v,
            Err(e) => {
                self.degrade_store(e, effects);
                return;
            }
        };
        let Some(state) = self.handles.get_mut(&id) else {
            return;
        };
        let current_sources: BTreeMap<EventId, BTreeSet<RelayUrl>> = current
            .iter()
            .map(|(id, row)| (*id, row.sources.clone()))
            .collect();
        if current_sources == state.last_rows && state.last_evidence.as_ref() == Some(&evidence) {
            return;
        }
        let mut delta: Vec<RowDelta> = Vec::new();
        for (event_id, row) in current {
            match state.last_rows.get(&event_id) {
                None => delta.push(RowDelta::Added(row)),
                Some(last_sources) if *last_sources != row.sources => {
                    delta.push(RowDelta::SourcesGrew {
                        id: event_id,
                        sources: row.sources,
                    });
                }
                Some(_) => {}
            }
        }
        for old_id in state.last_rows.keys() {
            if !current_sources.contains_key(old_id) {
                delta.push(RowDelta::Removed(*old_id));
            }
        }
        state.last_rows = current_sources;
        state.last_evidence = Some(evidence.clone());
        state.sink.on_rows(delta.clone());
        effects.push(Effect::EmitRows(id, delta, evidence));
    }

    /// The query's current matching row set (by id) + its
    /// [`AcquisitionEvidence`] -- an internal snapshot `refresh_handle`
    /// diffs against the handle's own remembered `last_rows` to compute the
    /// outgoing delta. This snapshot itself is never handed to a caller/
    /// effect directly.
    ///
    /// #124: when the demand carries a Nostr `limit:N` this projection is the
    /// N MOST RECENT matching rows -- `created_at` DESC, ties broken by event
    /// `id` ASC (bytewise), the NIP-01 canonical newest-first order -- NOT
    /// every cached match. The authoritative cap lives HERE, at the handle
    /// projection, deliberately NOT in `EventStore::query` (which must keep
    /// returning every current match: the resolver's `wide_concrete` KEEPS
    /// `limit`, so Derived-node recompute / negentropy / ingest all push
    /// limit-bearing filters into `query()` and rely on getting the FULL
    /// match set -- truncating there would corrupt reactive recompute).
    /// For this projection alone, each root atom may be pre-bounded through
    /// `EventStore::query_newest`; taking N newest from each atom is exact
    /// because a row outside one atom's top N already has N newer witnesses
    /// in that same atom. The final merged/deduped set is still capped ONCE,
    /// per NIP-01 per-subscription `limit` (see [`effective_row_limit`]).
    /// Because `refresh_handle` diffs THIS truncated snapshot against
    /// `last_rows`, the top-N is maintained reactively for free: a newer
    /// match entering the top-N evicts the oldest (Added(new)+Removed(oldest),
    /// never exceeding N), and retracting a top-N member pulls the next-newest
    /// in. `limit: None` is unchanged -- every match, no ordering imposed.
    /// Row truncation NEVER touches `evidence` below (coverage is about what
    /// was acquired, not how many rows are shown -- ledger #17): a limited
    /// query still records no coverage watermark.
    ///
    /// Rows are computed over `root_atoms` alone (delivery
    /// shape unchanged); evidence is computed over `subtree_atoms` (#12: the
    /// query's FULL subtree, interior `Derived` atoms included). Each row
    /// carries its provenance (#105: `StoredEvent::provenance`, already
    /// merged/persisted by `EventStore::insert`'s dedup path) rather than
    /// discarding it -- the mechanism already exists in `nmp-store`; this is
    /// only its honest projection.
    ///
    /// #107: `CacheMode::Strict` applies the pinned cache projection here --
    /// a cached row is returned only when its unioned provenance set
    /// intersects the handle's own pinned relay set (`Row.sources`, #105's
    /// existing field; no new store mechanism). This is read off THIS
    /// handle's own `QueryHandle::cache()`, never the shared graph node's --
    /// two handles sharing the identical (cache-free-deduped) acquisition
    /// key may still disagree on `cache` (Fable's ruling: cache is excluded
    /// from `AcquisitionKey`), so an Agnostic and a Strict handle over the
    /// same pinned selection MUST project different row sets despite
    /// sharing one graph/wire/coverage underneath. The pinned relay set
    /// itself comes from `subtree_atoms`' `source` -- Fable's ruling B
    /// ("uniform per Demand, not subtree") guarantees every atom in a
    /// single handle's subtree carries the SAME declared `SourceAuthority`,
    /// so any one atom's `source` is authoritative for the whole handle.
    /// `CacheMode::Strict` is only meaningful over a `SourceAuthority::
    /// Pinned` selection (the Contract: "pinned cache policy is part of
    /// source identity") -- over any other source there is no pinned relay
    /// set to intersect against, so Strict is a no-op there, identical to
    /// Agnostic.
    fn rows_and_evidence_for(
        &self,
        id: HandleId,
    ) -> Result<(BTreeMap<EventId, Row>, AcquisitionEvidence), PersistenceError> {
        let subtree_atoms = self.resolver.subtree_atoms(id);
        let pinned_relays: Option<&BTreeSet<RelayUrl>> = self
            .handles
            .get(&id)
            .filter(|state| state._handle.cache() == CacheMode::Strict)
            .and_then(|_| {
                subtree_atoms.iter().find_map(|atom| match &atom.source {
                    SourceAuthority::Pinned(relays) => Some(relays),
                    _ => None,
                })
            });

        let root_atoms = self.resolver.root_atoms(id);
        let row_limit = effective_row_limit(&root_atoms);
        let mut by_id: BTreeMap<EventId, Row> = BTreeMap::new();
        for atom in &root_atoms {
            #[cfg(test)]
            self.projection_store_queries
                .set(self.projection_store_queries.get().saturating_add(1));
            let filter = atom.to_nostr();
            let rows = match row_limit {
                Some(limit) => self.resolver.store().query_newest(&filter, limit)?,
                None => self.resolver.store().query(&filter)?,
            };
            for se in rows {
                if let Some(relays) = pinned_relays {
                    if !se
                        .provenance
                        .seen
                        .keys()
                        .any(|relay| relays.contains(relay))
                    {
                        continue;
                    }
                }
                by_id.entry(se.event.id).or_insert_with(|| Row {
                    event: se.event,
                    sources: se.provenance.seen.into_keys().collect(),
                });
            }
        }
        // #124: a demand carrying `limit:N` projects only its N newest rows.
        // Applied authoritatively to the merged/deduped set in NIP-01
        // canonical newest-first order. Each root atom was only pre-bounded
        // above; this final pass preserves the per-subscription (not
        // per-atom) contract. `refresh_handle`'s diff then maintains the
        // top-N reactively. No-op when there is no limit or the set fits.
        if let Some(limit) = row_limit {
            if by_id.len() > limit {
                let mut ordered: Vec<(u64, EventId)> = by_id
                    .iter()
                    .map(|(event_id, row)| (row.event.created_at.as_secs(), *event_id))
                    .collect();
                ordered.sort_by(|a, b| nip01_newest_first((a.0, &a.1), (b.0, &b.1)));
                let keep: BTreeSet<EventId> =
                    ordered.into_iter().take(limit).map(|(_, id)| id).collect();
                by_id.retain(|event_id, _| keep.contains(event_id));
            }
        }
        let evidence = evidence::acquisition_evidence(
            &subtree_atoms,
            self.router.plan(),
            self.resolver.store(),
            &self.connected_relays,
            &self.ever_connected_relays,
        );
        Ok((by_id, evidence))
    }
}

/// The demand's effective result cap (NIP-01 `limit:N`) -- the single limit
/// the app's subscription carries, to be applied ONCE to the final merged/
/// deduped row set the handle projects, never per-atom (#124). A demand fans
/// out into many `root_atoms` only via the cartesian product of its bound
/// fields' resolved elements (`Graph::compute_atoms`), and every one of those
/// atoms is a clone of the SAME base filter -- so they all carry the
/// IDENTICAL `limit`. Reducing with `max` over that invariantly-uniform set
/// is therefore just a defensive fold that yields exactly that shared value;
/// `None` iff the demand carried no limit at all (the whole set is projected,
/// unordered). For a union/multi-atom demand this is the deliberate choice:
/// NIP-01's `limit` is a property of the subscription, so the app sees the N
/// newest rows across the WHOLE union, not N per operand.
fn effective_row_limit(root_atoms: &BTreeSet<ConcreteFilter>) -> Option<usize> {
    // The uniform-limit invariant this fold rests on: every fanned root atom
    // is a clone of the same base filter, so they all carry the IDENTICAL
    // `limit`. `max` therefore returns exactly that shared value. If a future
    // graph change ever broke that assumption, `max` would silently
    // over-return (project the largest atom's N while smaller-N atoms wanted
    // fewer) -- so pin it here: a mixed-limit root set trips in tests rather
    // than degrading semantics in release (debug-only, zero release cost).
    debug_assert!(
        root_atoms
            .iter()
            .map(|atom| atom.limit)
            .collect::<BTreeSet<_>>()
            .len()
            <= 1,
        "root_atoms must share a single limit (NIP-01 limit is per-subscription); \
         got a mixed-limit set: {root_atoms:?}",
    );
    root_atoms.iter().filter_map(|atom| atom.limit).max()
}

/// The NIP-01 canonical newest-first total order used to pick the N most
/// recent rows for a `limit:N` demand (#124): `created_at` DESC, ties broken
/// by event `id` ASC compared bytewise -- the same deterministic order a
/// relay applies when it answers a limited REQ with "the `limit` most recent
/// events". Each argument is a `(created_at_secs, &id)` pair.
fn nip01_newest_first(a: (u64, &EventId), b: (u64, &EventId)) -> std::cmp::Ordering {
    b.0.cmp(&a.0)
        .then_with(|| a.1.as_bytes().cmp(b.1.as_bytes()))
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
mod receipt_allocator_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use nmp_router::FixtureDirectory;
    use nmp_store::MemoryStore;
    use nostr::{Keys, Kind};

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<WriteStatus>>>);

    impl ReceiptSink for Sink {
        fn on_status(&self, status: WriteStatus) {
            self.0.lock().unwrap().push(status);
        }
    }

    fn rejected_intent(keys: &Keys, created_at: u64) -> WriteIntent {
        WriteIntent {
            payload: WritePayload::Unsigned(UnsignedEvent::new(
                keys.public_key(),
                Timestamp::from(created_at),
                Kind::TextNote,
                vec![],
                "no active account",
            )),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
        }
    }

    #[test]
    fn last_upper_half_id_is_issued_once_then_exhaustion_is_stable_and_typed() {
        const FIRST_UNACCEPTED_ID: u64 = 1u64 << 63;
        let keys = Keys::generate();
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 10);
        core.set_next_unaccepted_receipt_for_test(Some(FIRST_UNACCEPTED_ID));

        let last_sink = Sink::default();
        let last = core.handle(EngineMsg::Publish(
            rejected_intent(&keys, 1),
            Box::new(last_sink.clone()),
        ));
        assert!(last.iter().any(|effect| {
            matches!(
                effect,
                Effect::EmitReceipt(ReceiptId(id), WriteStatus::Failed(_))
                    if *id == FIRST_UNACCEPTED_ID
            )
        }));
        assert!(matches!(
            last_sink.0.lock().unwrap().as_slice(),
            [WriteStatus::Failed(_)]
        ));

        for created_at in [2, 3] {
            let exhausted_sink = Sink::default();
            let exhausted = core.handle(EngineMsg::Publish(
                rejected_intent(&keys, created_at),
                Box::new(exhausted_sink.clone()),
            ));
            assert!(matches!(
                exhausted.as_slice(),
                [Effect::PublishFailed(
                    PublishError::ReceiptCorrelationIdExhausted
                )]
            ));
            assert!(exhausted_sink.0.lock().unwrap().is_empty());
            assert!(!exhausted
                .iter()
                .any(|effect| matches!(effect, Effect::EmitReceipt(..))));
        }

        assert_eq!(FIRST_UNACCEPTED_ID - 1, u64::MAX >> 1);
        assert!(core.pending.is_empty());
        assert!(core.resolver.store().recover_outbox().is_empty());
    }

    #[test]
    fn last_attempt_correlation_is_issued_once_then_exhaustion_is_stable_and_typed() {
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 10);
        core.set_next_attempt_correlation_for_test(Some(u64::MAX));

        assert_eq!(
            core.alloc_attempt_correlation(),
            Ok(AttemptCorrelation(u64::MAX))
        );
        assert_eq!(
            core.alloc_attempt_correlation(),
            Err(AttemptCorrelationExhausted)
        );
        assert_eq!(
            core.alloc_attempt_correlation(),
            Err(AttemptCorrelationExhausted),
            "exhaustion remains stable: no wrap, reuse, or fabricated id"
        );
    }

    #[test]
    fn attempt_correlation_exhaustion_precedes_lane_and_pending_mutation() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://correlation-exhausted.example").unwrap();
        let directory =
            FixtureDirectory::new().with_write(keys.public_key().to_hex(), [relay.clone()]);
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(directory), 10);
        core.handle(EngineMsg::SetActivePubkey(Some(keys.public_key())));
        let accepted = core.handle(EngineMsg::Publish(
            WriteIntent {
                payload: WritePayload::Unsigned(UnsignedEvent::new(
                    keys.public_key(),
                    Timestamp::from(93u64),
                    Kind::TextNote,
                    vec![],
                    "correlation boundary",
                )),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
            },
            Box::new(Sink::default()),
        ));
        let (receipt, generation, unsigned) = accepted
            .iter()
            .find_map(|effect| match effect {
                Effect::RequestSign(receipt, generation, unsigned) => {
                    Some((*receipt, *generation, unsigned.clone()))
                }
                _ => None,
            })
            .expect("accepted unsigned intent requests signing");
        let intent = core.pending[&receipt].intent_id.unwrap();
        core.set_next_attempt_correlation_for_test(None);

        let effects = core.handle(EngineMsg::SignerCompleted(
            receipt,
            generation,
            Ok(unsigned.sign_with_keys(&keys).unwrap()),
        ));

        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, Effect::PublishEvent(..))));
        assert!(core.attempt_correlations.is_empty());
        assert!(core.pending[&receipt].pending_relays.is_empty());
        assert!(core.pending[&receipt].attempt_ordinals.is_empty());
        assert!(core
            .resolver
            .store()
            .recover_attempts(intent)
            .unwrap()
            .is_empty());
        assert_eq!(
            core.alloc_attempt_correlation(),
            Err(AttemptCorrelationExhausted),
            "the failed call must not revive or wrap the namespace"
        );
    }
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
    use nostr::{EventBuilder, Keys, Kind, RelayMessage, SubscriptionId, Tag, Tags};

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
            RelayFrame::from(RelayMessage::event(SubscriptionId::new("s"), event)),
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

#[cfg(test)]
mod relay_admission_tests {
    //! Issue #121 falsifiers for the provenance-aware discovered-relay
    //! admission gate. All exercise the REAL `EngineCore::on_relay_frame`
    //! ingest path (a validly-signed kind:10002 delivered over the wire),
    //! never a bypassed direct directory poke -- the whole point is that a
    //! *validly signed but hostile* relay list is what we must reject.
    //!
    //! "Never reaches `ensure_open`" is proven structurally: a rejected relay
    //! is absent from `directory.write_relays`/`read_relays`, so the router
    //! never builds a candidate for it, so no `Effect` ever names it, so
    //! `runtime::dispatch_effect` never calls `pool.ensure_open` on it. Each
    //! test pins that absence at the directory, the choke point where a
    //! discovered relay would otherwise become a routable lane.

    use nmp_router::LiveDirectory;
    use nmp_store::MemoryStore;
    use nmp_transport::RelayFrame;
    use nostr::{EventBuilder, Keys, Kind, RelayMessage, SubscriptionId, Tag, Tags};

    // `RelayDirectory` (the trait whose `write_relays`/`read_relays` these
    // tests call) is already in scope via `use super::*` — importing it again
    // here is a redundant-import warning under `-D warnings`.
    use super::*;

    const SLOT: u32 = 0;
    const GEN: u64 = 1;

    fn relay(url: &str) -> RelayUrl {
        RelayUrl::parse(url).expect("valid test relay url")
    }

    /// Drive a signed kind:10002 (declaring every `url` as an unmarked
    /// read+write relay) through the engine's real ingest path.
    fn ingest_relay_list(core: &mut EngineCore<MemoryStore>, author: &Keys, urls: &[&RelayUrl]) {
        // A connected relay is the one the discovery frame arrives on.
        core.handle(EngineMsg::RelayConnected(
            TransportRelayHandle {
                slot: SLOT,
                generation: GEN,
            },
            relay("wss://indexer.example.com"),
        ));
        let tags: Vec<Tag> = urls
            .iter()
            .map(|u| Tag::relay_metadata((*u).clone(), None))
            .collect();
        let event = EventBuilder::new(Kind::RelayList, "")
            .tags(Tags::from_list(tags))
            .sign_with_keys(author)
            .expect("test fixture event must sign cleanly");
        core.handle(EngineMsg::RelayFrame(
            TransportRelayHandle {
                slot: SLOT,
                generation: GEN,
            },
            RelayFrame::from(RelayMessage::event(SubscriptionId::new("s"), event)),
        ));
    }

    fn admitted_writes(core: &EngineCore<MemoryStore>, author: &Keys) -> BTreeSet<RelayUrl> {
        core.directory
            .write_relays(&author.public_key().to_hex())
            .into_iter()
            .map(|lr| lr.url)
            .collect()
    }

    /// The headline falsifier: a validly-signed, network-DISCOVERED kind:10002
    /// listing a loopback, an RFC-1918, and a `.onion` relay alongside one
    /// public relay must admit ONLY the public relay. The three hostile
    /// relays never become lanes (so never reach `ensure_open`), and the
    /// diagnostic rejection counter records exactly them -- for BOTH the read
    /// and write parse of the one event (2.4's dual parse), i.e. 3 hosts ×
    /// 2 lanes = 6 rejections.
    #[test]
    fn discovered_private_and_onion_relays_are_rejected_and_counted() {
        let author = Keys::generate();
        let public = relay("wss://relay.example.com");
        let loopback = relay("ws://127.0.0.1:7777");
        let rfc1918 = relay("ws://10.0.0.5");
        let onion = relay("ws://expyuzz4wqqyqhjn.onion");

        // Secure default: empty allowlist.
        let mut core = EngineCore::new(
            MemoryStore::new(),
            Box::new(LiveDirectory::builder().build()),
            10,
        );
        ingest_relay_list(&mut core, &author, &[&public, &loopback, &rfc1918, &onion]);

        assert_eq!(
            admitted_writes(&core, &author),
            BTreeSet::from([public.clone()]),
            "only the public relay may become a discovered write lane"
        );
        let author_hex = author.public_key().to_hex();
        let admitted_reads: BTreeSet<RelayUrl> = core
            .directory
            .read_relays(&author_hex)
            .into_iter()
            .map(|lr| lr.url)
            .collect();
        assert_eq!(
            admitted_reads,
            BTreeSet::from([public]),
            "the read lane is gated identically -- no hostile host leaks in via read"
        );
        assert_eq!(
            core.discovered_private_relays_rejected, 6,
            "3 hostile hosts rejected on each of the write AND read parse of the one event"
        );
        assert_eq!(
            core.diagnostics_snapshot()
                .discovered_private_relays_rejected,
            6,
            "the rejection count must be visible in diagnostics (issue #121)"
        );
    }

    /// A user who EXPLICITLY opts a local host in re-admits a DISCOVERED relay
    /// on exactly that host -- provenance the transport layer lacks, which is
    /// why this decision lives in the engine. A different local host stays
    /// rejected.
    #[test]
    fn user_configured_local_host_admits_that_discovered_relay() {
        let author = Keys::generate();
        let opted_in = relay("ws://127.0.0.1:7777");
        let other_local = relay("ws://10.0.0.5");

        let mut core = EngineCore::new(
            MemoryStore::new(),
            Box::new(LiveDirectory::builder().build()),
            10,
        )
        .with_relay_admission(RelayAdmissionPolicy::new(["127.0.0.1".to_string()]));
        ingest_relay_list(&mut core, &author, &[&opted_in, &other_local]);

        assert_eq!(
            admitted_writes(&core, &author),
            BTreeSet::from([opted_in]),
            "the opted-in local host is admitted; a different local host is not"
        );
        assert_eq!(
            core.discovered_private_relays_rejected, 2,
            "only the non-opted-in local host is rejected -- once per lane parse"
        );
    }

    /// The "HOST, never path" falsifier at the engine layer: a real per-user
    /// relay served at a URL PATH is public and must be admitted from
    /// discovery, untouched by the SSRF gate.
    #[test]
    fn discovered_public_host_at_a_path_is_admitted() {
        let author = Keys::generate();
        let per_user = relay("wss://nostr.wine/npub1xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx");

        let mut core = EngineCore::new(
            MemoryStore::new(),
            Box::new(LiveDirectory::builder().build()),
            10,
        );
        ingest_relay_list(&mut core, &author, &[&per_user]);

        assert_eq!(
            admitted_writes(&core, &author),
            BTreeSet::from([per_user]),
            "a public host with a per-user path must pass admission -- the path is not a host"
        );
        assert_eq!(core.discovered_private_relays_rejected, 0);
    }
}

#[cfg(test)]
mod relay_health_tests {
    use super::*;
    use nmp_router::FixtureDirectory;
    use nmp_store::MemoryStore;

    #[test]
    fn verifier_outage_reaches_engine_diagnostics_without_false_misbehavior() {
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 10);
        let health = RelayHealth {
            last_error: Some("signature verification worker unavailable".to_string()),
            invalid_signature_count: 0,
            ..RelayHealth::default()
        };

        let effects = core.handle(EngineMsg::RelayHealth(7, health));
        assert!(effects.iter().any(|effect| {
            matches!(effect, Effect::EmitDiagnostics(snapshot)
                if snapshot.transport_degraded.as_deref()
                    == Some("signature verification worker unavailable"))
        }));
        assert_eq!(
            core.diagnostics_snapshot().transport_degraded.as_deref(),
            Some("signature verification worker unavailable")
        );
    }
}

#[cfg(test)]
mod affected_handle_invalidation_tests {
    use std::sync::{Arc, Mutex};

    use nmp_grammar::IndexedTagName;
    use nmp_router::FixtureDirectory;
    use nmp_store::MemoryStore;
    use nostr::{EventBuilder, Keys, Kind, Tag};

    use super::*;

    const HANDLE_COUNT: usize = 64;
    const ROWS_PER_HANDLE: usize = 4;

    #[derive(Clone, Default)]
    struct CapturingSink(Arc<Mutex<Vec<Vec<RowDelta>>>>);

    impl RowSink for CapturingSink {
        fn on_rows(&self, rows: Vec<RowDelta>) {
            self.0.lock().unwrap().push(rows);
        }
    }

    fn room_event(keys: &Keys, room: usize, ordinal: usize, created_at: u64) -> SignedEvent {
        EventBuilder::new(Kind::from(9u16), format!("room-{room}-event-{ordinal}"))
            .tag(Tag::parse(["h".to_owned(), format!("room-{room}")]).unwrap())
            .custom_created_at(Timestamp::from(created_at))
            .sign_with_keys(keys)
            .unwrap()
    }

    fn room_query_for_kind(room: usize, kind: u16, limit: usize) -> LiveQuery {
        LiveQuery::from_filter(Filter {
            kinds: Some(BTreeSet::from([kind])),
            tags: BTreeMap::from([(
                IndexedTagName::new('h').unwrap(),
                Binding::Literal(BTreeSet::from([format!("room-{room}")])),
            )]),
            limit: Some(limit),
            ..Filter::default()
        })
    }

    fn room_query(room: usize) -> LiveQuery {
        room_query_for_kind(room, 9, 200)
    }

    #[test]
    fn ordinary_room_batch_queries_only_the_matching_handle_and_skips_router_compile() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://affected-room.example").unwrap();
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 20);

        let mut seed = Vec::new();
        for room in 0..HANDLE_COUNT {
            for ordinal in 0..ROWS_PER_HANDLE {
                let event = room_event(
                    &keys,
                    room,
                    ordinal,
                    (room * ROWS_PER_HANDLE + ordinal + 1) as u64,
                );
                seed.push((
                    event,
                    RelayObserved::new(relay.clone(), Timestamp::from(1u64)),
                ));
            }
        }
        core.resolver.store_mut().insert_batch(seed).unwrap();

        let sinks: Vec<_> = (0..HANDLE_COUNT)
            .map(|room| {
                let sink = CapturingSink::default();
                core.handle(EngineMsg::Subscribe(
                    room_query(room),
                    Box::new(sink.clone()),
                ));
                sink
            })
            .collect();

        core.projection_store_queries.set(0);
        core.router_compiles.set(0);
        for sink in &sinks {
            sink.0.lock().unwrap().clear();
        }

        let arriving = room_event(&keys, 17, 99, 50_000);
        let mut effects = Vec::new();
        core.ingest_relay_events(
            vec![(
                arriving.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(50_001u64)),
            )],
            &mut effects,
        );

        assert_eq!(core.projection_store_queries.get(), 1);
        assert_eq!(core.router_compiles.get(), 0);
        for (room, sink) in sinks.iter().enumerate() {
            let batches = sink.0.lock().unwrap();
            if room == 17 {
                assert_eq!(batches.len(), 1);
                assert!(matches!(
                    batches[0].as_slice(),
                    [RowDelta::Added(row)] if row.event.id == arriving.id
                ));
            } else {
                assert!(batches.is_empty(), "unrelated room {room} was refreshed");
            }
        }

        // A byte-for-byte duplicate observation is a true no-op: no handle
        // query and no router compile merely to rediscover that fact.
        core.projection_store_queries.set(0);
        core.router_compiles.set(0);
        let mut duplicate_effects = Vec::new();
        core.ingest_relay_events(
            vec![(
                arriving.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(50_001u64)),
            )],
            &mut duplicate_effects,
        );
        assert_eq!(core.projection_store_queries.get(), 0);
        assert_eq!(core.router_compiles.get(), 0);
        assert!(duplicate_effects
            .iter()
            .all(|effect| !matches!(effect, Effect::EmitRows(..))));

        // The same id from a genuinely new relay changes only provenance.
        // It must re-query the one matching handle, emit SourcesGrew there,
        // and still avoid both unrelated handles and router compilation.
        for sink in &sinks {
            sink.0.lock().unwrap().clear();
        }
        core.projection_store_queries.set(0);
        core.router_compiles.set(0);
        let second_relay = RelayUrl::parse("wss://second-room-source.example").unwrap();
        let mut provenance_effects = Vec::new();
        core.ingest_relay_events(
            vec![(
                arriving.clone(),
                RelayObserved::new(second_relay.clone(), Timestamp::from(50_002u64)),
            )],
            &mut provenance_effects,
        );
        assert_eq!(core.projection_store_queries.get(), 1);
        assert_eq!(core.router_compiles.get(), 0);
        for (room, sink) in sinks.iter().enumerate() {
            let batches = sink.0.lock().unwrap();
            if room == 17 {
                assert_eq!(batches.len(), 1);
                assert!(matches!(
                    batches[0].as_slice(),
                    [RowDelta::SourcesGrew { id, sources }]
                        if *id == arriving.id
                            && *sources == BTreeSet::from([relay.clone(), second_relay.clone()])
                ));
            } else {
                assert!(batches.is_empty(), "unrelated room {room} was refreshed");
            }
        }
    }

    #[test]
    fn top_n_insert_queries_only_its_handle_and_emits_eviction_delta() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://top-n-affected.example").unwrap();
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 20);
        let oldest = room_event(&keys, 7, 0, 10);
        let retained = room_event(&keys, 7, 1, 20);
        let unrelated = room_event(&keys, 8, 0, 10);
        core.resolver
            .store_mut()
            .insert_batch(
                [oldest.clone(), retained, unrelated]
                    .into_iter()
                    .map(|event| {
                        (
                            event,
                            RelayObserved::new(relay.clone(), Timestamp::from(30u64)),
                        )
                    })
                    .collect(),
            )
            .unwrap();

        let affected = CapturingSink::default();
        let other = CapturingSink::default();
        core.handle(EngineMsg::Subscribe(
            room_query_for_kind(7, 9, 2),
            Box::new(affected.clone()),
        ));
        core.handle(EngineMsg::Subscribe(
            room_query_for_kind(8, 9, 2),
            Box::new(other.clone()),
        ));
        affected.0.lock().unwrap().clear();
        other.0.lock().unwrap().clear();
        core.projection_store_queries.set(0);

        let newest = room_event(&keys, 7, 2, 40);
        let mut effects = Vec::new();
        core.ingest_relay_events(
            vec![(
                newest.clone(),
                RelayObserved::new(relay, Timestamp::from(41u64)),
            )],
            &mut effects,
        );

        assert_eq!(core.projection_store_queries.get(), 1);
        let batches = affected.0.lock().unwrap();
        assert_eq!(batches.len(), 1);
        assert!(batches[0]
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == newest.id)));
        assert!(batches[0]
            .iter()
            .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == oldest.id)));
        assert!(other.0.lock().unwrap().is_empty());
    }

    #[test]
    fn replaceable_supersession_invalidates_old_and_new_matches_only() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://replaceable-affected.example").unwrap();
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 20);
        let replaceable = |room: usize, created_at: u64| {
            EventBuilder::new(Kind::from(10_000u16), format!("winner-{room}"))
                .tag(Tag::parse(["h".to_owned(), format!("room-{room}")]).unwrap())
                .custom_created_at(Timestamp::from(created_at))
                .sign_with_keys(&keys)
                .unwrap()
        };
        let old = replaceable(3, 10);
        core.resolver
            .store_mut()
            .insert_batch(vec![(
                old.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(11u64)),
            )])
            .unwrap();

        let old_sink = CapturingSink::default();
        let new_sink = CapturingSink::default();
        let unrelated_sink = CapturingSink::default();
        for (room, sink) in [
            (3, old_sink.clone()),
            (4, new_sink.clone()),
            (5, unrelated_sink.clone()),
        ] {
            core.handle(EngineMsg::Subscribe(
                room_query_for_kind(room, 10_000, 10),
                Box::new(sink.clone()),
            ));
            sink.0.lock().unwrap().clear();
        }
        core.projection_store_queries.set(0);

        let new = replaceable(4, 20);
        let mut effects = Vec::new();
        core.ingest_relay_events(
            vec![(
                new.clone(),
                RelayObserved::new(relay, Timestamp::from(21u64)),
            )],
            &mut effects,
        );

        assert_eq!(core.projection_store_queries.get(), 2);
        assert!(matches!(
            old_sink.0.lock().unwrap().as_slice(),
            [batch] if matches!(batch.as_slice(), [RowDelta::Removed(id)] if *id == old.id)
        ));
        assert!(matches!(
            new_sink.0.lock().unwrap().as_slice(),
            [batch] if matches!(batch.as_slice(), [RowDelta::Added(row)] if row.event.id == new.id)
        ));
        assert!(unrelated_sink.0.lock().unwrap().is_empty());
    }

    #[test]
    fn kind_five_removed_row_invalidates_matching_handle_without_shape_luck() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://deletion-affected.example").unwrap();
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 20);
        let target = room_event(&keys, 12, 0, 10);
        core.resolver
            .store_mut()
            .insert_batch(vec![(
                target.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(11u64)),
            )])
            .unwrap();

        let affected = CapturingSink::default();
        let unrelated = CapturingSink::default();
        core.handle(EngineMsg::Subscribe(
            room_query(12),
            Box::new(affected.clone()),
        ));
        core.handle(EngineMsg::Subscribe(
            room_query(13),
            Box::new(unrelated.clone()),
        ));
        affected.0.lock().unwrap().clear();
        unrelated.0.lock().unwrap().clear();
        core.projection_store_queries.set(0);

        let deletion = EventBuilder::new(Kind::EventDeletion, "")
            .tag(Tag::event(target.id))
            .custom_created_at(Timestamp::from(20u64))
            .sign_with_keys(&keys)
            .unwrap();
        let mut effects = Vec::new();
        core.ingest_relay_events(
            vec![(deletion, RelayObserved::new(relay, Timestamp::from(21u64)))],
            &mut effects,
        );

        assert_eq!(core.projection_store_queries.get(), 1);
        assert!(matches!(
            affected.0.lock().unwrap().as_slice(),
            [batch] if matches!(batch.as_slice(), [RowDelta::Removed(id)] if *id == target.id)
        ));
        assert!(unrelated.0.lock().unwrap().is_empty());
    }

    #[test]
    fn resolver_internal_handle_is_filtered_before_any_projection_read() {
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 20);
        let (internal, _delta) = core.resolver.subscribe(room_query(1)).unwrap();
        core.projection_store_queries.set(0);

        let mut effects = Vec::new();
        core.refresh_handles([internal.id()], &mut effects);

        assert_eq!(core.projection_store_queries.get(), 0);
        assert!(effects.is_empty());
    }
}
