//! The PURE synchronous reducer (plan §2 position 1, §3.4). `EngineCore`
//! owns the M1 resolver `Engine<S>`, the M2 `Router`, the write-delivery
//! state, and the coverage-attribution bookkeeping (`attribution.rs`,
//! `evidence.rs`). Its entire surface is:
//!
//! ```ignore
//! impl<S: EventStore> EngineCore<S> {
//!     pub fn handle(&mut self, msg: EngineMsg) -> Vec<Effect>;
//!     pub fn tick(&mut self, now: nostr::Timestamp) -> Vec<Effect>;
//!     pub fn next_deadline(&self) -> Result<Option<nostr::Timestamp>, PersistenceError>;
//! }
//! ```
//!
//! The deadline door reads two durable indexes, so it can fail; `Ok(None)`
//! means the driver has genuinely nothing to wake up for and never that the
//! store could not be read (#763).
//!
//! `EngineCore` does NO I/O, spawns no threads, touches no socket, imposes
//! no runtime — this is the seam that preserves M1/M2's headless property:
//! the whole engine's logic is testable by feeding `EngineMsg`s and
//! asserting `Effect`s, with zero network (plan §5 tier A).
//!
//! Coverage attribution follows
//! `docs/design/query-demand-and-evidence.md` plus issue #816's
//! request-scoped facts-before-claims contract: send-time snapshots + the
//! FIFO intersection rule live in [`attribution`]; the per-query, per-source
//! acquisition evidence (`rows + compact facts, never a collapsed global
//! verdict` — `docs/design/scoped-evidence-49-12-plan.md`, folding #12 into
//! #49) lives in [`evidence`]. Both are engine-owned — the store
//! (`nmp-store`) only stores whatever interval it is handed.

mod admission;
#[cfg(test)]
mod admission_tests;
mod attribution;
#[cfg(test)]
mod auth_core_headless;
mod auth_transport;
mod diagnostics;
mod evidence;
#[cfg(test)]
mod handoff_starvation_tests;
mod history;
mod history_lifecycle;
#[cfg(test)]
mod history_lifecycle_tests;
#[cfg(test)]
mod lane_bootstrap_retry_tests;
mod lane_projection;
mod observation;
#[cfg(test)]
mod outbox_tests;
mod query;
#[cfg(test)]
mod query_tests;
#[cfg(test)]
mod transport_tests;
mod write;
#[cfg(test)]
mod write_tests;

#[cfg(any(test, feature = "bench-instrumentation"))]
use std::cell::Cell;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use nostr::{
    filter::MatchEventOptions, Event as SignedEvent, EventBuilder as NostrEventBuilder, EventId,
    PublicKey, RelayMessage, RelayUrl, Timestamp, UnsignedEvent,
};

use nmp_grammar::{
    fold_byte, AccessContext, CacheMode, ConcreteFilter, ContextualAtom, DescriptorHash, Freshness,
    Identity, LiveQuery, RelaySessionKey, RoutingEvidence, SourceAuthority, WriteIntent,
    WritePayload, WriteRouting,
};
use nmp_resolver::{
    CommittedCurrentRow, CommittedMutationResult, CommittedRowChanges, Engine as ResolverEngine,
    HandleId, LocalAcceptResult, QueryHandle, RelayIngestError,
};
use nmp_router::{
    AdvertisedRelayLimits, AuthorRouteState, AuthorRoutes, CompileBudget, RelayPlan, Router,
    RoutingFacts, RuleRegistry, SubId, WireDelta, WireOp, WireReq,
};
use nmp_signer::SignerError;
use nmp_store::{
    sentinel_signature, AcceptOutcome, AcceptWrite, AuthDenial as StoredAuthDenial,
    AuthDenialSource as StoredAuthDenialSource, CloseIntentOutcome, CompensateOutcome, CoverageKey,
    DurabilityOutcome, EventStore, HandoffEvidence, IntentId, IntentSigState, PersistenceError,
    PromoteOutcome, PublishQueueAttemptHandoff, PublishQueueAttemptOutcome,
    PublishQueueDeadlineKind, PublishQueueInFlightPhase, PublishQueueLane, PublishQueueLaneKey,
    PublishQueueLaneState, PublishQueuePostHandoffState, PublishQueueTerminalOutcome,
    PublishQueueTransientCause, ReceiptState, RelayObserved, RemoveQueueEntryOutcome,
    VerifiedSignature,
};
use nmp_transport::{
    AttemptCorrelation, CommittedObservationCandidate, CommittedObservationHit,
    CommittedObservationPublication, DisconnectReason, HandoffResult, RelayFrame,
    RelayHandle as TransportRelayHandle, RelayHealth,
};

use crate::negentropy::{NegStep, ProbedRelay, Prober, Reconciler};
use crate::publish_queue::{
    AuthDenialSource, CancelWriteError, CancelWriteOutcome, NotSentReason, PublishQueueEntry,
    RelayState, RelayWaiting, RemoveQueueEntryError, RetryCause, SigningState, WriteFact,
    WriteOutcome,
};
use crate::relay_information_service::RelayInformationCapabilityEvidence;

/// The liveness deadline (plan §4/harvest `nmp-nip77`) past which an open
/// negentropy session with no reply is abandoned in favor of a plain REQ
/// (never left to hang forever, and never silently re-tried as negentropy
/// again on the same generation -- `tick`'s own staleness sweep is the only
/// caller of this constant).
const NEG_LIVENESS_DEADLINE_SECS: u64 = 30;

// Internal wire-id roles for the gap-free NIP-77 handoff (#563). They are
// folded onto the router-owned plan id plus the exact full filter hash, so a
// live candidate, NEG session, missing-id fetch, and ordinary fallback can
// coexist on one websocket without aliasing either NIP-01's or NIP-77's
// subscription namespace.
const NIP77_LIVE_ROLE: u8 = 0x71;
const NIP77_NEG_ROLE: u8 = 0x72;
const NIP77_MISSING_ROLE: u8 = 0x73;
const NIP77_FALLBACK_ROLE: u8 = 0x74;

/// The engine's private, in-memory implementation of the router's read-only
/// neutral fact view. There is no persistence door for `authors`, so
/// session-derived absence necessarily returns to `Unknown` on restart.
pub(crate) struct RoutingFactStore {
    authors: BTreeMap<PublicKey, AuthorRouteState>,
    operator_app: Vec<RelayUrl>,
    operator_fallback: Vec<RelayUrl>,
}

impl RoutingFactStore {
    pub(crate) fn new(
        operator_app: impl IntoIterator<Item = RelayUrl>,
        operator_fallback: impl IntoIterator<Item = RelayUrl>,
    ) -> Self {
        Self {
            authors: BTreeMap::new(),
            operator_app: operator_app.into_iter().collect(),
            operator_fallback: operator_fallback.into_iter().collect(),
        }
    }

    #[allow(dead_code)]
    fn writer(&mut self) -> AuthorRouteWriter<'_> {
        AuthorRouteWriter { facts: self }
    }

    pub(crate) fn from_fixture(fixture: nmp_router::FixtureRoutingFacts) -> Self {
        let (authors, operator_app, operator_fallback) = fixture.into_parts();
        Self {
            authors,
            operator_app,
            operator_fallback,
        }
    }
}

impl Default for RoutingFactStore {
    fn default() -> Self {
        Self::new([], [])
    }
}

impl RoutingFacts for RoutingFactStore {
    fn author_routes(&self, author: &PublicKey) -> AuthorRouteState {
        self.authors.get(author).cloned().unwrap_or_default()
    }

    fn operator_app_relays(&self) -> Vec<RelayUrl> {
        self.operator_app.clone()
    }

    fn operator_fallback_relays(&self) -> Vec<RelayUrl> {
        self.operator_fallback.clone()
    }
}

/// The only values accepted by the private author-route mutation door.
/// `Unknown` is intentionally absent: it is solely cold-start state.
#[allow(dead_code)]
pub(crate) enum AuthorRouteReplacement {
    Present(AuthorRoutes),
    Absent,
}

/// Borrowed, non-cloneable writer capability. One call replaces the complete
/// directional fact, so no observer can see a mixed old/new pair.
#[allow(dead_code)]
pub(crate) struct AuthorRouteWriter<'a> {
    facts: &'a mut RoutingFactStore,
}

impl AuthorRouteWriter<'_> {
    #[allow(dead_code)]
    pub(crate) fn replace(&mut self, author: PublicKey, replacement: AuthorRouteReplacement) {
        let state = match replacement {
            AuthorRouteReplacement::Present(routes) => AuthorRouteState::Present(routes),
            AuthorRouteReplacement::Absent => AuthorRouteState::Absent,
        };
        self.facts.authors.insert(author, state);
    }
}

#[cfg(test)]
mod routing_fact_store_tests {
    use super::*;
    use nostr::Keys;

    #[test]
    fn one_write_replaces_both_directions_atomically() {
        let author = Keys::generate().public_key();
        let old_outbound = RelayUrl::parse("wss://old-outbound.example").unwrap();
        let old_inbound = RelayUrl::parse("wss://old-inbound.example").unwrap();
        let new_outbound = RelayUrl::parse("wss://new-outbound.example").unwrap();
        let new_inbound = RelayUrl::parse("wss://new-inbound.example").unwrap();
        let mut facts = RoutingFactStore::default();

        facts.writer().replace(
            author,
            AuthorRouteReplacement::Present(AuthorRoutes::new([old_outbound], [old_inbound])),
        );
        facts.writer().replace(
            author,
            AuthorRouteReplacement::Present(AuthorRoutes::new(
                [new_outbound.clone()],
                [new_inbound.clone()],
            )),
        );

        let AuthorRouteState::Present(routes) = facts.author_routes(&author) else {
            panic!("replacement must remain positive knowledge");
        };
        assert_eq!(routes.outbound(), &BTreeSet::from([new_outbound]));
        assert_eq!(routes.inbound(), &BTreeSet::from([new_inbound]));
    }

    #[test]
    fn absence_is_memory_only_and_a_later_positive_record_wins() {
        let author = Keys::generate().public_key();
        let relay = RelayUrl::parse("wss://later-positive.example").unwrap();
        let mut facts = RoutingFactStore::default();

        facts
            .writer()
            .replace(author, AuthorRouteReplacement::Absent);
        assert_eq!(facts.author_routes(&author), AuthorRouteState::Absent);

        facts.writer().replace(
            author,
            AuthorRouteReplacement::Present(AuthorRoutes::new([relay.clone()], [])),
        );
        assert_eq!(
            facts.author_routes(&author),
            AuthorRouteState::Present(AuthorRoutes::new([relay], []))
        );
        assert_eq!(
            RoutingFactStore::default().author_routes(&author),
            AuthorRouteState::Unknown,
            "a fresh process cannot inherit session-derived absence"
        );
    }
}

/// Derive the wire id for one NIP-77 role subscription, in the ENGINE's own
/// per-connection wire-string namespace.
///
/// `incarnation` is what makes each derivation a FRESH string (#932), and it
/// is the whole point of this signature. Role + plan id + filter hash alone
/// are content-derived, so closing a role subscription and later re-deriving
/// it for the same plan id and the same filter reproduced the SAME 64-hex
/// wire string. `AttributionState::discard_sub` drops the closed
/// incarnation's inflight FIFO and its wire mapping, so the re-derived string
/// re-registered with a FRESH FIFO -- and a straggler EOSE for the
/// pre-Close REQ then popped the NEW snapshot and minted durable coverage for
/// a request the relay had not finished serving. Coverage is what
/// `plan_is_fresh_for` trusts, so that over-claimed acquisition outright.
///
/// This is the derived-namespace counterpart of what #899/#912 did for
/// PLANNED subscriptions, whose ids are allocated tokens the router never
/// recycles within a session (`nmp_router::SubId::allocate`). It deliberately
/// lives here rather than in `AttributionState::record_send`: the plan path
/// must NOT be incarnated, because `Effect::Replay` ships `WireReq`s straight
/// out of the router's plan and `on_wire_request_handoff` keys on
/// `(session, sub_id)` -- a blanket mint at send time would break that
/// router-to-engine correspondence. Only ids the engine both mints and
/// stores itself can carry an incarnation, and these four roles are exactly
/// those ids.
///
/// The incarnation is FOLDED INTO the digest, never appended to it: a
/// `SubId`'s wire string is the hex `Display` of this hash, fixed at 64
/// characters, which is exactly NIP-01's `subscription_id` cap. Real relays
/// that declare `max_subid_length` sit at 71 and most declare nothing, so 64
/// is the ceiling to respect and an incarnation marker has to fit inside the
/// existing hash rather than extend it.
fn nip77_role_sub_id(
    plan_sub_id: &SubId,
    role: u8,
    filter: &ConcreteFilter,
    incarnation: u64,
) -> SubId {
    let mut hash = fold_byte(plan_sub_id.1, role);
    for byte in filter.hash().as_bytes() {
        hash = fold_byte(hash, *byte);
    }
    for byte in incarnation.to_be_bytes() {
        hash = fold_byte(hash, byte);
    }
    SubId(plan_sub_id.0.clone(), hash, plan_sub_id.2)
}

const RETRY_INITIAL_SECS: u64 = 3;
const RETRY_MAX_SECS: u64 = 300;
const RETRY_JITTER_MAX_SECS: u64 = 5;
const ACK_TIMEOUT_SECS: u64 = 30;
/// NIP-42 permits an authentication event at most ten minutes from relay
/// receipt. We spend that future window as a checked per-live-session nonce
/// when repeated identical challenges arrive inside one reducer second.
const AUTH_MAX_FUTURE_SECS: u64 = 600;
/// Never minted by `mint_auth_sequence`; owned exclusively by the
/// counter-exhausted fallback `AuthEpoch` (phase `Error`) so sentinel and
/// real epochs are distinct BY VALUE, not merely by phase.
const AUTH_SEQUENCE_SENTINEL: u64 = u64::MAX;
const MAX_GLOBAL_ATTEMPTS: usize = 32;
/// The two shapes a local-persistence stall takes, kept as exact strings so
/// an operator reading `RelayWaiting::PersistenceStalled` learns which one
/// happened. They differ only in whether the resolved relay URL survives a
/// crash — a recovery detail, not an app decision, which is why it rides
/// `detail` rather than a second variant.
/// Why an attempt was replaced without ever having been acknowledged or
/// refused: nothing is left in this process that could deliver its transport
/// handoff, so the attempt is abandoned in favour of a fresh one rather than
/// held open against a reply that cannot come (#1316).
const ORPHANED_HANDOFF_DETAIL: &str =
    "no transport handoff is outstanding for this attempt; the identical frozen event is \
     republished under a new attempt";
const ATTEMPT_STALL_DETAIL: &str =
    "the durable attempt fact could not be committed; no wire EVENT was emitted and recovery \
     rediscovers this exact relay from its committed route revision";
const ROUTE_STALL_DETAIL: &str =
    "the append-only route revision could not be committed; this exact relay URL is not claimed \
     to survive a crash";
const DEADLINE_READ_BATCH: usize = 1_024;

fn retry_delay_secs(key: &PublishQueueLaneKey, ordinal: u64) -> u64 {
    let exponent = ordinal.saturating_sub(1).min(63) as u32;
    let base = RETRY_INITIAL_SECS
        .checked_shl(exponent)
        .unwrap_or(u64::MAX)
        .min(RETRY_MAX_SECS);

    // FNV-1a is used as a deliberately tiny, fully specified stable hash.
    // Jitter is policy spreading, not a security boundary; unlike
    // DefaultHasher this remains identical across processes and releases.
    let mut hash = 0xcbf29ce484222325u64;
    for byte in key
        .intent_id
        .0
        .to_be_bytes()
        .into_iter()
        .chain(key.relay.as_str().as_bytes().iter().copied())
        .chain(ordinal.to_be_bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    base.saturating_add(hash % RETRY_JITTER_MAX_SECS)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RelayAckClass {
    Acked,
    Transient(PublishQueueTransientCause),
    WaitingAuth,
    Rejected,
}

fn classify_relay_ack(status: bool, message: &str) -> RelayAckClass {
    if status {
        return RelayAckClass::Acked;
    }
    let Some((prefix, _)) = message.split_once(':') else {
        return RelayAckClass::Rejected;
    };
    match prefix {
        "duplicate" => RelayAckClass::Acked,
        "rate-limited" => RelayAckClass::Transient(PublishQueueTransientCause::RelayRateLimited),
        "error" => RelayAckClass::Transient(PublishQueueTransientCause::RelayError),
        "auth-required" => RelayAckClass::WaitingAuth,
        "invalid" | "pow" | "blocked" | "restricted" | "mute" => RelayAckClass::Rejected,
        _ => RelayAckClass::Rejected,
    }
}

pub use admission::{RelayAdmissionPolicy, RelayRefusal};
use attribution::{
    AttributionSendId, AttributionState, CompletedAttribution, CoveragePoison, EventFailureTarget,
};
use diagnostics::{stalled_write_id, STALLED_WRITE_DETAIL_LIMIT};
pub use diagnostics::{
    AuthDiagnosticsPhase, AuthDiagnosticsSnapshot, DiagnosticsSnapshot, FilterCoverageEntry,
    RelayDiagnosticsSnapshot, StalledWrite, StalledWriteStage, StalledWriteTotals,
};
pub use evidence::{AcquisitionEvidence, AuthPhase, ShortfallFact, SourceEvidence, SourceStatus};
pub use history::{HistoryAdvanceError, HistoryBatch, HistoryQuery, HistorySessionId, WindowLoad};
pub use nmp_network_policy::{Declarer, OnionReachability};
use observation::{
    ActiveRequestEvidence, LiveWireRequest, ObservationExecutionState, PendingRequestEvidence,
};
pub use observation::{
    ObservationEvidence, ObservationFact, RequestTerminal, ResolutionCause, ResolvedBindingValue,
};
pub use query::Nip77Frame;
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

/// Opaque, identity-stable continuation for finite receipt replay pages
/// (#680). This is delivery mechanism, not a third app noun: callers can
/// only return it to the same receipt's continuation door.
///
/// State is bounded by the receipt's finite relay fan-out, not by retry
/// history. Each relay keeps one durable attempt-fact high-water mark and
/// one current-lane revision; receipt and pending-state projections keep
/// constant-size/set markers over those same bounded relays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptReplayCursor {
    state: Box<ReceiptReplayCursorState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReceiptReplayCursorState {
    receipt_id: ReceiptId,
    receipt_status: Option<WriteFact>,
    awaiting_capability: bool,
    /// Whether the destination picture has been replayed for this receipt.
    destinations: bool,
    attempts: BTreeMap<RelayUrl, ReceiptAttemptReplayKey>,
    lane_revisions: BTreeMap<RelayUrl, u64>,
    /// The relays whose persistence stall has been replayed. Latched, never
    /// cleared: a fault a later ack papered over is still the fault.
    persistence_stalled: BTreeSet<(RelayUrl, write::PersistenceStallKind)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ReceiptAttemptReplayKey {
    ordinal: u64,
    phase: ReceiptAttemptReplayPhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ReceiptAttemptReplayPhase {
    Handoff,
    Transient,
    Outcome,
}

impl ReceiptReplayCursor {
    pub(crate) fn new(receipt_id: ReceiptId) -> Self {
        Self {
            state: Box::new(ReceiptReplayCursorState {
                receipt_id,
                receipt_status: None,
                awaiting_capability: false,
                destinations: false,
                attempts: BTreeMap::new(),
                lane_revisions: BTreeMap::new(),
                persistence_stalled: BTreeSet::new(),
            }),
        }
    }
}

/// The two, and only two, ways `publish()` refuses.
///
/// Everything else takes CUSTODY and fails in the queue where the app can
/// see it — no relays, no signer online, a stale replaceable base and disk
/// trouble mid-flight all become queue entries rather than errors here.
///
/// **Rule 1 — NMP cannot write anything down.** No ink:
/// [`Self::EngineShuttingDown`] and [`Self::PersistenceFailed`] — which is
/// also where receipt-id exhaustion arrives, because the ONLY receipt-id
/// space left is the store's own durable allocator inside the acceptance
/// transaction, and running it out fails that transaction like any other
/// write. The separate upper-half "unaccepted" namespace is deleted with
/// the stream-local failure receipt it existed to identify.
///
/// **Rule 2 — the instruction cannot resolve.** Nothing in this class is a
/// fact about the world; each is "you asked for something unanswerable," so
/// nothing is pinned and nothing may park:
/// [`Self::NoActiveAccount`], [`Self::SignatureInvalid`],
/// [`Self::IdentityContradictsSignedAuthor`], [`Self::ReservedKind`],
/// [`Self::EmptyExplicitRoute`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishError {
    /// The runtime has begun its finite cancellation/drain phase and cannot
    /// accept a new write before closing.
    EngineShuttingDown,
    /// The acceptance transaction itself failed. Recording the failure would
    /// need the disk that just refused, so there is no queue entry to fail
    /// into.
    PersistenceFailed { reason: String },
    /// [`Identity::Active`](nmp_grammar::Identity::Active) with no account
    /// active. Nothing is pinned, so nothing may park — and a later login
    /// could sign as the wrong person.
    NoActiveAccount,
    /// A caller-supplied [`WritePayload::Signed`](nmp_grammar::WritePayload)
    /// whose signature does not verify. Rare by construction: NMP applies
    /// the signature itself for a builder payload.
    SignatureInvalid { reason: String },
    /// An explicit identity naming somebody other than a signed payload's
    /// own author. A contradiction with no correct resolution.
    IdentityContradictsSignedAuthor {
        identity: PublicKey,
        author: PublicKey,
    },
    /// A kind the reducer owns and no app may publish (kind:22242 is relay
    /// authentication).
    ReservedKind { kind: u16 },
    /// [`WriteRouting::Explicit`](nmp_grammar::WriteRouting) naming no
    /// relays. It never degrades into `Auto`: sending a write to relays the
    /// caller did not choose is the failure this refusal exists to prevent.
    EmptyExplicitRoute,
}

impl std::fmt::Display for PublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EngineShuttingDown => write!(f, "engine is shutting down"),
            Self::PersistenceFailed { reason } => {
                write!(f, "the write could not be recorded: {reason}")
            }
            Self::NoActiveAccount => write!(
                f,
                "publishing as the active account requires an active account"
            ),
            Self::SignatureInvalid { reason } => {
                write!(f, "the supplied signature does not verify: {reason}")
            }
            Self::IdentityContradictsSignedAuthor { identity, author } => write!(
                f,
                "explicit identity {identity} does not match the signed event author {author}"
            ),
            Self::ReservedKind { kind } => write!(
                f,
                "kind:{kind} is reserved for reducer-owned relay authentication"
            ),
            Self::EmptyExplicitRoute => write!(
                f,
                "an explicit route naming no relays is refused: it never degrades into Auto"
            ),
        }
    }
}

impl std::error::Error for PublishError {}

/// Truthful result of trying to attach a receipt observer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReattachOutcome {
    /// The retained receipt and all replay evidence were readable.
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

/// One pure, finite durable-receipt replay result. Core calculates facts and
/// its identity-stable continuation; runtime alone decides whether and where
/// to deliver them or register a live mailbox.
#[derive(Debug)]
pub struct ReceiptReplayPage {
    pub outcome: ReattachOutcome,
    pub facts: Vec<WriteFact>,
    /// `Some` means another finite replay call is required before joining
    /// live delivery. `None` means this page reached current durable truth.
    pub next_cursor: Option<ReceiptReplayCursor>,
    /// Final cursor after every fact in this page. Runtime retains this only
    /// when the whole page entered the consumer mailbox.
    pub(crate) end_cursor: Option<ReceiptReplayCursor>,
    /// The frozen event id of the receipt this page replayed, read from the
    /// same durable record. `Some` exactly when `outcome` is `Attached`: an
    /// absent or unreadable receipt has no identity to report. A
    /// correlation-idempotent republish resolves to an existing obligation
    /// instead of accepting a new one, and this is where its acceptance
    /// answer gets the same event id a first acceptance returns.
    pub(crate) frozen_id: Option<EventId>,
    /// #961: each entry advances only the matching fact over the page's input
    /// cursor. Runtime uses this to checkpoint one accepted live effect
    /// without accidentally acknowledging another effect returned by the
    /// same reducer mutation.
    pub(crate) isolated_fact_cursors: Vec<ReceiptReplayCursor>,
}

impl ReceiptReplayPage {
    /// Whether replay reached a live receipt that can continue producing facts.
    pub fn is_attached(&self) -> bool {
        self.outcome.is_attached()
    }

    fn unavailable(outcome: ReattachOutcome) -> Self {
        debug_assert!(outcome != ReattachOutcome::Attached);
        Self {
            outcome,
            facts: Vec::new(),
            next_cursor: None,
            end_cursor: None,
            frozen_id: None,
            isolated_fact_cursors: Vec::new(),
        }
    }
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

impl Row {
    /// The relay hint a reference row to this event carries.
    ///
    /// `sources` is **verified** provenance: NMP observed this exact event at
    /// those relays, and since #1221 the set means "relays that hold it", not
    /// "whatever delivered it first". That is the honest thing to put in a
    /// hint slot that has, across the entire tree before #1243, been filled
    /// exactly once.
    ///
    /// Which of several verified sources is the BEST hint is deliberately
    /// still open (#1243's design record, "where relay hints come from"). The
    /// better answer than either single fact is a relay present in both the
    /// seen set and the author's declared NIP-65 outbox — welshman prefers
    /// declared for staleness reasons, quartz tracks nothing and takes hints
    /// from the caller, and NMP is unusual in holding both facts. That
    /// computation needs NIP-65, which `nmp-grammar` cannot reach, so it
    /// belongs at the publish door or in the app rather than folded in here.
    /// Until it exists this is the first source in sorted order, which is
    /// deterministic rather than arbitrary; an app that knows better states
    /// its own with `from_relay`.
    fn verified_hint(&self) -> Option<RelayUrl> {
        self.sources.iter().next().cloned()
    }
}

/// The canonical row is the ordinary reply/quote/reaction target, so it is
/// what `EventBuilder::tag` is usually handed.
///
/// A `Row` adds exactly ONE thing to the bare signed event `nmp-grammar`
/// already knows how to point at: the verified relay hint. Everything else —
/// the thread-position reading, the letter, the author slot, the companion
/// `p` row, the carried mentions and the dedup — is grammar's, delegated to
/// rather than restated, so a `Row` and a bare `nostr::Event` can never drift
/// into two dialects.
impl nmp_grammar::RootScope for Row {
    fn root_rows(&self, options: &nmp_grammar::TagOptions) -> Vec<nostr::Tag> {
        nmp_grammar::event_root_rows(&self.event, self.verified_hint(), options)
    }

    fn parent_rows(&self, options: &nmp_grammar::TagOptions) -> Vec<nostr::Tag> {
        nmp_grammar::event_parent_rows(&self.event, self.verified_hint(), options)
    }

    fn entity_kind(&self) -> Option<nostr::Kind> {
        Some(self.event.kind)
    }
}

/// A row-set delta (plan §7 non-goal: no ordering/windowing in M3 — raw
/// deltas + coverage only). This is the standard reactive-query contract:
/// `Effect::EmitRows` NEVER re-sends the query's full
/// current row set -- only the rows ADDED and REMOVED since that handle's
/// LAST emit (`refresh_observation`'s job). The FIRST emit for a fresh subscribe
/// is "every currently-matching row, as `Added`" (there is nothing to diff
/// against yet); an identity re-root (`set_active_pubkey`) that swaps the
/// whole row set falls out of the SAME diff -- "remove everything old, add
/// everything new" -- with no special-casing. Without this contract, a
/// long-running subscription that keeps matching new events re-delivers its
/// ENTIRE growing row set on every single ingest: O(rows) work per event,
/// O(rows²) total over a session (confirmed live: ~3.35M raw row deliveries
/// for ~2,587 distinct notes in 20s against real relays --
/// `docs/known-gaps.md`'s P0).
///
/// Runtime delivery may compose several of these reducer deltas into one
/// exact transition rebased onto the observer's last delivered batch (#46);
/// that preserves this incremental contract while bounding a slow observer's
/// pending backlog.
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
    /// lifecycle event forced a `refresh_observation` recompute of this one.
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

/// Identity of one reducer-owned NIP-42 challenge epoch. The sequence is
/// monotonic for the exact physical session and is never reset by a new
/// transport generation; the handle makes stale-generation completions
/// structurally distinguishable even before the sequence is inspected.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AuthEpoch {
    pub handle: TransportRelayHandle,
    pub session: RelaySessionKey,
    pub sequence: u64,
}

/// One asynchronous operation inside an [`AuthEpoch`]. Tokens are minted in
/// monotonic order per exact session and are never inferred from challenge
/// text, event ids, the active account, or callback arrival order.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AuthOpToken {
    pub epoch: AuthEpoch,
    pub sequence: u64,
}

/// App-owned policy's explicit result for one exact AUTH operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthPolicyOutcome {
    Allow,
    Deny { reason: String },
    Unavailable,
    Error { reason: String },
}

/// Signer adapter's explicit result for one exact AUTH operation. A signed
/// event is still untrusted until the reducer verifies the complete frozen
/// template, id, and signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthSignerOutcome {
    Signed(SignedEvent),
    Unavailable,
    Rejected { reason: String },
    Error { reason: String },
}

/// Result of handing the reducer-validated AUTH event to the exact current
/// physical session. This correlation is intentionally separate from the
/// durable-write [`AttemptCorrelation`] namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthSendOutcome {
    Accepted,
    Unavailable,
}

/// The one, ever, terminal of one exact AUTH send, translated verbatim from
/// transport's `PoolEvent::EphemeralHandoff` (issue #883).
///
/// Transport never runs engine code: it emits this value on the ordinary pool
/// event path and the reducer applies it on its own owner thread. The exact
/// `(handle, session)` the frame was submitted against travels WITH the
/// terminal, so `EngineCore::on_auth_send_completed` re-derives the awaiting
/// [`AuthOpToken`] from state it already owns instead of keeping a side table
/// of in-flight completions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthSendCompletion {
    pub handle: TransportRelayHandle,
    pub session: RelaySessionKey,
    /// The opaque transport operation token this send was started with —
    /// exactly [`AuthOpToken::sequence`], which is minted per engine and
    /// never reused.
    pub operation: u64,
    pub outcome: AuthSendOutcome,
}

impl AuthSendCompletion {
    /// The terminal for the exact send `token` started. Used where the token
    /// is still in hand — transport's synchronous refusal path, and headless
    /// reducer tests that drive a completion directly.
    #[must_use]
    pub fn for_operation(token: &AuthOpToken, outcome: AuthSendOutcome) -> Self {
        Self {
            handle: token.epoch.handle,
            session: token.epoch.session.clone(),
            operation: token.sequence,
            outcome,
        }
    }
}

/// Capability whose removal/replacement invalidates AUTH truth for the
/// frozen expected key. Runtime registries send this after their own exact
/// registration identity check; the reducer never consults mutable current
/// account state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthCapability {
    Policy,
    Signer,
}

/// Opaque identity of one exact registered policy or signer capability.
/// Registries mint this identity; stale removal of an older instance cannot
/// invalidate a replacement because the reducer compares the instance
/// frozen into the current epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AuthCapabilityInstance(pub u64);

/// The complete reducer-to-runtime AUTH executor vocabulary. Runtime owns
/// execution and cancellation; only the reducer owns epoch truth and phase
/// transitions.
#[derive(Debug)]
pub enum AuthEffect {
    Cancel(AuthEpoch),
    RequestPolicy {
        token: AuthOpToken,
        expected_pubkey: PublicKey,
        challenge: String,
    },
    RequestSignature {
        token: AuthOpToken,
        unsigned: Box<UnsignedEvent>,
    },
    Send {
        token: AuthOpToken,
        event: Box<SignedEvent>,
    },
}

/// The read/write/frame vocabulary the reducer consumes (plan §3.4).
pub enum EngineMsg {
    Subscribe(LiveQuery),
    /// Execute relay-bound demand admitted during the current short cohort.
    /// Runtime owns the monotonic deadline and supplies wall-clock truth for
    /// liveness state minted by this transition; the reducer owns both the
    /// admission transition and those stamps. This advances clock truth but
    /// never runs deadline maintenance.
    FlushWireAdmission(Timestamp),
    Unsubscribe(ObservationId),
    SubscribeHistory(HistoryQuery),
    /// Declaratively raise this window's row target to at least `usize`,
    /// clamped to the declared `max_rows` (#485). Monotonic and idempotent:
    /// a value at or below the current target is a no-op (or, at the bound, a
    /// single `AtBound` frame beat). Replaces the opaque continuation token.
    RequestRows(HistorySessionId, usize),
    /// Runtime acknowledgement that every newly-required relay worker was
    /// acquired and the staged window advance may become observable.
    CommitHistoryLoad(HistorySessionId),
    /// Runtime refusal/caller cancellation before a staged advance became
    /// observable. Restores the exact prior projection and demand.
    RollbackHistoryLoad(HistorySessionId),
    UnsubscribeHistory(HistorySessionId),
    SetActivePubkey(Option<PublicKey>),
    Publish(WriteIntent),
    RelayConnected(TransportRelayHandle, RelaySessionKey),
    /// Transport completed this exact protected generation's initial socket
    /// observation. Any observed frame was ordered before this edge on the
    /// same worker event stream; public generations never emit it.
    AuthProbeReleased(TransportRelayHandle, RelaySessionKey),
    /// Result of the engine-owned NIP-11 one-shot started for a connected
    /// relay. `Some` retains document revision/freshness/error provenance;
    /// `None` means no document fact was acquired before the decision grace.
    /// Deliberately URL-keyed: NIP-11 is one-shot HTTP evidence about the
    /// relay itself, acquired outside any websocket session (#8: only the
    /// PUBLIC session ever consumes it).
    RelayInformationResolved(RelayUrl, Option<RelayInformationCapabilityEvidence>),
    /// `reason` distinguishes an ordinary transient disconnect (the pool
    /// itself keeps redialing on its own backoff schedule -- the reducer's
    /// job is only to reflect the link status and re-request its worker) from
    /// a `DisconnectReason::PermanentlyFailed` one (401/403 -- the pool has
    /// ALREADY retired the worker for good; see `on_relay_disconnected`'s
    /// doc for why a permanent reason must never re-issue `Effect::
    /// EnsureRelay`, which would otherwise busy-loop against a relay that
    /// keeps saying no) and a `DisconnectReason::Closed` one (an intentional
    /// close must never resurrect the session).
    RelayDisconnected(TransportRelayHandle, RelaySessionKey, DisconnectReason),
    RelayHealth(TransportRelayHandle, RelaySessionKey, RelayHealth),
    /// Runtime could not create a required relay worker. Observational only:
    /// current demand remains the retry owner and diagnostics retain the
    /// exact failure instead of silently presenting a merely connecting
    /// session forever.
    RelayOpenFailed(RelaySessionKey, String),
    RelayFrame(TransportRelayHandle, RelaySessionKey, RelayFrame),
    RelayFrames(Vec<(TransportRelayHandle, RelaySessionKey, RelayFrame)>),
    SignerCompleted(ReceiptId, u64, Result<SignedEvent, SignerError>),
    /// The runtime has no signer attached for this accepted author. This is
    /// non-terminal: the canonical pending row and durable obligation stay
    /// alive until a matching signer is attached or the app cancels.
    SignerUnavailable(ReceiptId, u64),
    /// A capability for this author was attached. Re-arm every matching
    /// accepted unsigned intent through the ordinary RequestSign effect.
    SignerAttached(PublicKey),
    AuthPolicyCompleted(
        AuthOpToken,
        Option<AuthCapabilityInstance>,
        AuthPolicyOutcome,
    ),
    AuthSignerCompleted(
        AuthOpToken,
        Option<AuthCapabilityInstance>,
        AuthSignerOutcome,
    ),
    /// Runtime atomically snapped this exact capability instance before
    /// starting the asynchronous operation named by `token`. Binding is a
    /// reducer input, not inferred from whichever instance later completes.
    AuthCapabilityBound {
        token: AuthOpToken,
        capability: AuthCapability,
        instance: AuthCapabilityInstance,
    },
    AuthSendCompleted(AuthSendCompletion),
    AuthCapabilityInvalidated(PublicKey, AuthCapability, AuthCapabilityInstance),
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

/// One explicit, serialized observation-opening result.
///
/// The reducer proves the initial canonical projection before it commits any
/// router or sibling-observer effects. Success therefore carries the exact
/// registered owner and first mailbox value; refusal carries only facts that
/// remain true after the speculative resolver owner has been rolled back.
/// Runtime never infers either outcome by searching an effect list.
pub(crate) enum ObservationOpen<Id, Seed> {
    Opened {
        id: Id,
        seed: Seed,
        effects: Vec<Effect>,
    },
    Refused {
        reason: String,
        effects: Vec<Effect>,
    },
}

pub(crate) struct RowsSeed {
    pub(crate) deltas: Vec<RowDelta>,
    /// Per-BRANCH acquisition evidence in canonical branch order (#1108).
    pub(crate) evidence: Vec<AcquisitionEvidence>,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CoreObservationOwnershipCensus {
    pub(crate) handles: usize,
    pub(crate) histories: usize,
    pub(crate) history_handles: usize,
    pub(crate) resolver_nodes: usize,
    pub(crate) demand_atoms: usize,
    pub(crate) planned_sessions: usize,
    pub(crate) pending_execution_owners: usize,
    pub(crate) active_execution_owners: usize,
    pub(crate) live_wire_owners: usize,
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
    /// Arm one first-arrival-anchored wire-admission deadline. Repeated arms
    /// while a deadline is pending do not extend it.
    ArmWireAdmission,
    /// Update the transport's volatile exact-observation eligibility only
    /// from durable post-commit facts. Invalidations are applied before
    /// publications by the cache.
    UpdateCommittedObservations {
        invalidated: Vec<EventId>,
        published: Vec<CommittedObservationPublication>,
    },
    /// The complete current set of public keys whose neutral author-route
    /// provider is still needed. Most are `Unknown`; zero-destination writes
    /// also retain settled zero-route contributors so a later positive
    /// replacement can unpark them. This is a need declaration, never a
    /// subscription; optional protocol assembly owns any exact query it opens.
    AuthorRouteNeedsChanged(BTreeSet<PublicKey>),
    /// -> `Pool::send` per (relay, current handle).
    Wire(WireDelta),
    /// Reconnect: resend the current wire subs on the NEW generation of
    /// exactly this session.
    Replay(RelaySessionKey, Vec<WireReq>),
    /// Acquire/revalidate NIP-11 without blocking the reducer thread.
    FetchRelayInformation(RelayUrl),
    /// Open the exact protected transport generation's ordinary outbound gate
    /// after its ordered initial-read edge is applied, or required AUTH
    /// completes.
    ReleaseInitialRead(TransportRelayHandle),
    /// Place a capability-probing `NEG-OPEN` on the wire (`negentropy::
    /// Prober::begin_probe`'s output, carried in full since the runtime has
    /// no negentropy-protocol knowledge of its own): the sub-id, the
    /// throwaway probe filter, and the hex initial message.
    StartProbe(RelayUrl, SubId, ConcreteFilter, String),
    /// Place a real `NEG-OPEN` after the live-first EOSE barrier for
    /// `filter` against a PROVEN-supported relay (ledger #8's compile-fence:
    /// the first field can only ever be a `ProbedRelay`), under its own
    /// NIP-77 `sub_id`, with the initial message built from the local store.
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
    /// One observation's merged row transition plus its per-BRANCH
    /// acquisition evidence, indexed by canonical branch order (#1108). A
    /// single-branch live query carries exactly one entry; nothing here is
    /// ever rolled up into a global verdict across branches.
    EmitRows(ObservationId, Vec<RowDelta>, Vec<AcquisitionEvidence>),
    /// Ordered observation-scoped execution facts. Runtime folds these into
    /// the same bounded observation mailbox as rows and acquisition facts.
    EmitObservationEvidence(ObservationId, Vec<ObservationEvidence>),
    EmitHistory(HistorySessionId, HistoryBatch),
    HistoryLoadResult(HistorySessionId, Result<(), HistoryAdvanceError>),
    /// The engine-global diagnostics projection (M5 plan §1.2 step 3),
    /// pushed at the end of every `recompile()` and after every EOSE
    /// (coverage watermarks can advance with no recompile at all). Read-only
    /// and off the data path -- never influences routing/delivery.
    /// `runtime::Handle::observe_diagnostics` forwards this to every
    /// registered observer, latest-wins if a consumer is slow (never
    /// buffered/replayed).
    EmitDiagnostics(DiagnosticsSnapshot),
    EmitReceipt(ReceiptId, WriteFact),
    /// `publish()` took CUSTODY: the write is durably recorded under this
    /// receipt id and whatever becomes of it will be too. Not a fact on the
    /// stream — acceptance is what the `Ok` return already says — so nothing
    /// downstream delivers it to an observer.
    ///
    /// Carries the event id acceptance FROZE alongside the receipt id it
    /// issued, because both were decided by the same transaction: the id is
    /// re-derived inside `on_publish` when the acceptance transaction moves a
    /// replaceable edit's stamp, so what travels here is the post-restamp
    /// value in every case.
    ///
    /// Custody is not viability: a write can be in custody and already
    /// permanently failed.
    WriteAccepted(ReceiptId, EventId),
    /// A correlation-idempotent publish resolved to an existing receipt.
    /// These retained facts are not new live transitions: runtime must prime
    /// only that publish caller's fresh mailbox, then join it to live delivery
    /// at the page's final cursor. Existing observers must never receive this
    /// replay.
    ReplayReceipt(ReceiptId, ReceiptReplayPage),
    /// `publish()` refused. Nothing durable exists and nothing ever will —
    /// see [`PublishError`] for the only two reasons that is ever true.
    PublishFailed(PublishError),
    RequestSign(ReceiptId, u64, UnsignedEvent),
    /// Execute one reducer-owned NIP-42 operation. This envelope has its own
    /// epoch/token and never reuses durable-write signing or handoff
    /// correlations.
    RelayAuth(AuthEffect),
    /// A remote signer became available again before its previous retryable
    /// completion reached the engine. The runtime checks the currently
    /// registered capability's live availability before sending the ordinary
    /// `SignerAttached` event, closing that cross-thread ordering race.
    RearmSignerIfAvailable(PublicKey),
    RequestDecrypt(EventId, PublicKey, String),
    /// Delivery: publish `event` to `relay` (plan §3.4's "`Effect::Wire`
    /// publish REQ/EVENT per relay", re-cut as its OWN effect rather than a
    /// `nmp_router::WireOp` variant — `WireOp`/`WireDelta` are read-
    /// subscription vocabulary owned by `nmp-router`, out of this builder's
    /// scope to extend; this is engine-owned wire vocabulary for the write
    /// plane). C (runtime) translates this to `Pool::send_durable` of an
    /// `["EVENT", …]` frame on `relay`'s current generation, correlated by
    /// `AttemptCorrelation` (issue #93) — the durable handoff is generation-
    /// scoped and reports back exactly one typed `HandoffResult`, never
    /// silently carried into a later connection. Since the AUTH-reducer wave
    /// (#8 U2) the write plane rides the lane's identity-scoped
    /// authenticated session — `RelaySessionKey::new(relay,
    /// AccessContext::Nip42(signing pubkey))` — never the relay's Public
    /// read session: the reducer that can actually authenticate that
    /// session now exists, and an OK is only ever trusted from the exact
    /// session the write was published on.
    PublishEvent(RelaySessionKey, SignedEvent, AttemptCorrelation),
    /// Ensure a read-owned relay session is dialing. This effect carries no
    /// durable-write priority: a protected read cannot displace another
    /// physical session merely because its access context is non-Public.
    EnsureReadRelay(RelaySessionKey),
    /// Ensure a write-owned relay session is dialing without creating an
    /// attempt. An ordinal is allocated only after `RelayConnected` proves
    /// the session online, so offline time consumes zero attempts. Keeping
    /// this distinct from [`Self::EnsureReadRelay`] makes same-relay
    /// time-sharing authority a reducer-issued capability, never a runtime
    /// guess from the session's access context (#598).
    EnsureWriteRelay(RelaySessionKey),
}

/// One coherent reducer snapshot of physical relay-worker ownership.
///
/// `all` is the exact close/retain set. `writes` is its subset backed by a
/// nonterminal durable or ephemeral write obligation; only that subset may
/// time-share a same-relay Public slot under the physical-session cap (#598).
pub(crate) struct RelayWorkerRequirements {
    pub(crate) all: BTreeSet<RelaySessionKey>,
    pub(crate) writes: BTreeSet<RelaySessionKey>,
}

/// Per-handle bookkeeping `EngineCore` must retain across `handle()` calls:
/// the `QueryHandle` itself (dropping it would withdraw the subscription —
/// see `nmp_resolver::QueryHandle`'s `Drop` impl) and the last-emitted
/// row/evidence state (so `EmitRows` fires only when
/// something actually changed, not on every unrelated recompile).
/// `AcquisitionEvidence` derives `PartialEq` precisely so this
/// change-detection compare stays a plain value comparison, as the former
/// query-evidence aggregate's did. `last_rows` maps each currently-matching
/// id to the SOURCE SET last emitted for it (#105) -- not just the id --
/// so `refresh_observation` can detect provenance growth on an already-matching
/// row the SAME way it already detects `Added`/`Removed`: a plain value
/// compare against this remembered state, never a second bespoke mechanism.
struct BranchState {
    _handle: QueryHandle,
    acquisition: HandleAcquisition,
    observation: ObservationId,
    /// This branch's index in its observation's canonical branch order. It
    /// is what keeps a value resolved at one branch from ever being read as
    /// another branch's evidence.
    index: usize,
    execution: ObservationExecutionState,
}

/// The opaque identity of ONE live observation (#1108).
///
/// An observation owns one or more complete [`nmp_grammar::Demand`] branches
/// and delivers exactly one frame stream for all of them. It is the key every
/// mailbox, cancellation and emitted row/evidence effect uses; a resolver
/// `HandleId` names one BRANCH beneath it and is never an app-facing
/// observation identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObservationId(pub(crate) u64);

/// One live observation's delivered projection: the merged row union across
/// its branches, the per-branch acquisition evidence last delivered for it,
/// and the observation-monotonic execution sequence.
///
/// `last_rows` is the state actually delivered — the union of every branch's
/// matching rows by event id, with provenance merged, after the declared
/// aggregate result limit is applied ONCE to that union. It is never `N` rows
/// per branch.
struct ObservationState {
    /// Branch handles in canonical branch order. Never empty.
    branches: Vec<HandleId>,
    /// The declared bound on the merged row union, applied after the union.
    aggregate_result_limit: Option<usize>,
    last_rows: BTreeMap<EventId, RememberedRow>,
    last_evidence: Option<Vec<AcquisitionEvidence>>,
    /// False after any failed full refresh. Direct deltas cannot repair a
    /// possibly missed historical snapshot, so the next affected batch must
    /// retry the full oracle before incremental application resumes.
    projection_complete: bool,
    /// Monotonic within the OBSERVATION, never per branch: the app receives
    /// one ordered execution trace for the whole live query.
    next_sequence: u64,
}

/// The immutable opening-time result of every Demand boundary in one
/// observation handle. The vector follows the resolver's stable structural
/// Demand order (root first), so reactive value changes update the atoms
/// without overwriting which boundary owns which policy decision.
struct HandleAcquisition {
    scopes: Vec<ScopeAcquisition>,
}

/// One Demand boundary's freshness decision. Lifecycle ownership is
/// represented by variants, never a teardown bool: only `Live` contributes
/// that boundary's current atoms to the router; a coverage-satisfied scope
/// retains the exact plan that justified suppression.
enum ScopeAcquisition {
    Live,
    CoverageSatisfied(RelayPlan),
    CacheOnly(RelayPlan),
}

impl ScopeAcquisition {
    fn contributes_wire(&self) -> bool {
        matches!(self, Self::Live)
    }

    fn evidence_plan(&self) -> Option<&RelayPlan> {
        match self {
            Self::CoverageSatisfied(plan) | Self::CacheOnly(plan) => Some(plan),
            Self::Live => None,
        }
    }
}

impl HandleAcquisition {
    fn root(&self) -> Option<&ScopeAcquisition> {
        self.scopes.first()
    }
}

struct HistoryState {
    query: HistoryQuery,
    /// One opening-time acquisition decision per canonical branch, in branch
    /// order. Branches own independent freshness policy, so one branch may
    /// suppress remote work from its own persisted coverage while another
    /// contributes live work; neither borrows the other's decision.
    acquisitions_by_branch: Vec<HandleAcquisition>,
    /// Resolver handles the session currently holds open: the one live-top
    /// demand (`live_handle_id`) plus at most the *current* advance's
    /// tie-second/older acquisition handles. Older advances' historical
    /// acquisitions are closed at the next commit (#486) so a deep scroll of
    /// `K` advances never accumulates `O(K)` live relay subscriptions.
    handles: Vec<QueryHandle>,
    handle_ids: BTreeSet<HandleId>,
    /// The initial, permanent live-top demand opened for each canonical
    /// branch at [`Self::on_subscribe_history`], in branch order. None of
    /// these is ever a historical acquisition; they are retired only when
    /// the whole session is dropped.
    live_handle_ids: Vec<HandleId>,
    /// Which canonical branch each resolver handle this session holds open
    /// belongs to — live-top, tie-second and older-range alike. Evidence and
    /// demand scopes are grouped by this, never by declaration order.
    branch_of: BTreeMap<HandleId, usize>,
    /// Every engine-owned acquisition handle the session currently holds open,
    /// mapped to `Some(second)` for a tie-second REQ (`since==until==second`)
    /// or `None` for an older-range REQ. The live-top handle is never in this
    /// map. This is what the #486 supersede-close consults: an older handle is
    /// always safe to retire once superseded (its range is re-requestable, so
    /// no permanent gap), while a tie handle is kept open until the window
    /// boundary descends strictly below its second — only then is that dense
    /// second fully materialized as an interior region and its REQ redundant,
    /// so retiring it can never drop an un-projected same-second row.
    acquisitions: BTreeMap<HandleId, Option<u64>>,
    target_rows: usize,
    acquired_tie_seconds: BTreeSet<u64>,
    /// The bounded canonical payload set. History delivery is latest-wins,
    /// so every emitted frame must be able to stand alone after intermediate
    /// deltas are overwritten.
    last_rows: BTreeMap<EventId, Row>,
    /// Same membership as `last_rows`, ordered canonically newest-first.
    /// This makes top/bottom rebalance O(log max_rows), never an O(total)
    /// sort after every committed row mutation.
    order: BTreeSet<(Reverse<u64>, EventId)>,
    /// Per-BRANCH acquisition evidence in canonical branch order (#1108).
    last_evidence: Option<Vec<AcquisitionEvidence>>,
    projection_complete: bool,
    load: WindowLoad,
    pending_load: Option<PendingHistoryLoad>,
}

struct PendingHistoryLoad {
    prior_target_rows: usize,
    prior_load: WindowLoad,
    prior_evidence: Option<Vec<AcquisitionEvidence>>,
    prior_projection_complete: bool,
    acquired_tie_second: Option<u64>,
    opened_handle_ids: Vec<HandleId>,
    added_row_ids: Vec<EventId>,
    staged_batches: Vec<HistoryBatch>,
}

/// The minimal retained projection state needed to apply a committed writer
/// delta without re-materializing the handle's entire history. Event bodies
/// still live only in the store/app delta; the engine remembers selection and
/// provenance keys, not a second payload cache.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RememberedRow {
    created_at: u64,
    sources: BTreeSet<RelayUrl>,
}

/// Per-receipt bookkeeping the reducer retains from `Publish` through to the
/// last per-relay ack (or `Ephemeral`'s generation-scoped handoff effects).
/// Ephemeral still owns a receipt-only record and status stream; what it
/// lacks is a publish queue obligation and canonical pending row.
#[derive(Clone)]
struct QuarantinedWrite {
    intent_id: IntentId,
    frozen: SignedEvent,
}

/// Reducer-owned, rebuildable view of one intent's durable lane rows.
///
/// The store remains authoritative. `persisted` supports exact reverse-index
/// cleanup, `nonterminal` answers steady-state worker demand, and `uncertain`
/// is a conservative superset used only when a commit may have landed but its
/// post-state was not observable. Uncertainty may keep a worker alive; it may
/// never cause a possibly durable lane to lose its worker.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct LaneWorkerProjection {
    persisted: BTreeSet<RelayUrl>,
    nonterminal: BTreeSet<RelayUrl>,
    uncertain: BTreeSet<RelayUrl>,
}

impl LaneWorkerProjection {
    fn from_recovered(lanes: &[PublishQueueLane]) -> Self {
        let mut projection = Self::default();
        for lane in lanes {
            projection.apply(lane);
        }
        projection
    }

    /// Apply one exact committed post-state. Returns whether this relay was
    /// newly learned as a persisted lane and therefore needs reverse indexing.
    fn apply(&mut self, lane: &PublishQueueLane) -> bool {
        let relay = lane.key.relay.clone();
        let newly_persisted = self.persisted.insert(relay.clone());
        self.uncertain.remove(&relay);
        if matches!(lane.state, PublishQueueLaneState::Terminal { .. }) {
            self.nonterminal.remove(&relay);
        } else {
            self.nonterminal.insert(relay);
        }
        newly_persisted
    }

    fn mark_uncertain(&mut self, relay: RelayUrl) -> bool {
        self.uncertain.insert(relay.clone());
        self.persisted.insert(relay)
    }

    fn required_relays(&self) -> impl Iterator<Item = &RelayUrl> {
        self.nonterminal.iter().chain(&self.uncertain)
    }

    fn can_close(&self) -> bool {
        !self.persisted.is_empty() && self.nonterminal.is_empty() && self.uncertain.is_empty()
    }
}

/// One intent's outstanding lane-bootstrap gap.
///
/// Bootstrap is both the create-if-missing lane mutation and the only
/// complete read that establishes the projection, so a failure leaves the
/// reducer unable to name this intent's durable lanes. Retention is handled
/// conservatively at the moment of failure; this record is what makes that
/// retention *temporary* rather than permanent.
#[derive(Clone, Debug, PartialEq, Eq)]
struct LaneBootstrapRetry {
    /// The route candidates conservatively held in
    /// `LaneWorkerProjection::uncertain` until bootstrap commits.
    ///
    /// `None` means the intent's durable route set could not itself be read,
    /// so no per-relay `uncertain` marking can cover the gap and the whole
    /// projection must report unavailable instead. Unknown is sticky: a
    /// later failure that does know its candidates cannot upgrade an already
    /// unknown route set into a covered one.
    candidates: Option<BTreeSet<RelayUrl>>,
    /// Wall-clock instant at which `tick` retries. `next_deadline` folds this
    /// in, so the existing runtime timer drives the retry; nothing scans.
    due: Timestamp,
    /// Consecutive bootstrap failures, feeding the same capped exponential
    /// backoff shape the durable lane retry schedule uses.
    failures: u32,
}

impl LaneBootstrapRetry {
    /// Whether the conservative marking taken at the moment of failure can
    /// stand in for this intent's unknown durable lane set.
    ///
    /// An empty candidate set marks nothing, so it covers nothing: it is
    /// exactly as blind as an unreadable route set and gets the same
    /// retain-everything treatment.
    fn covers_retention(&self) -> bool {
        self.candidates
            .as_ref()
            .is_some_and(|candidates| !candidates.is_empty())
    }
}

/// Capped exponential backoff for lane-bootstrap retries.
///
/// Deliberately the same shape as [`retry_delay_secs`] without its per-lane
/// jitter: there is exactly one bootstrap in flight per intent, so there is
/// no thundering herd across ordinals to spread.
fn bootstrap_retry_delay_secs(failures: u32) -> u64 {
    RETRY_INITIAL_SECS
        .checked_shl(failures.saturating_sub(1).min(63))
        .unwrap_or(u64::MAX)
        .min(RETRY_MAX_SECS)
}

struct PendingWrite {
    routing: WriteRouting,
    /// False only when a persisted routing snapshot cannot be decoded.
    /// Recovery keeps owning the obligation but fails closed on wire output.
    routing_valid: bool,
    /// Store-allocated durable intent id. Every write in `pending` has one:
    /// a refusal at the acceptance door never enters `pending` at all, it
    /// becomes a terminal-at-birth queue entry instead.
    intent_id: IntentId,
    /// The instant this obligation was accepted -- the exact value written
    /// to `AcceptWrite::accepted_at` and replayed by
    /// `PublishQueueIntent::accepted_at`, so it is one durable fact rather than
    /// a process-local stopwatch. `stalled_writes` projects it verbatim;
    /// nothing here ever turns it into a duration, because a duration frozen
    /// into a snapshot goes stale the moment the snapshot stops being
    /// re-emitted while the instant never does.
    accepted_at: Timestamp,
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
    /// Routed lanes for which `start_lane_attempt` failed. They remain
    /// explicitly owned and nonterminal, but never enter `pending_relays`
    /// because no Started fact exists and no wire EVENT was emitted.
    unstarted_relays: BTreeSet<RelayUrl>,
    /// Resolved URLs whose route revision did not persist. Owned only for
    /// this process lifetime; crash recovery may re-resolve policy but cannot
    /// claim these exact URLs durably.
    route_blocked_relays: BTreeSet<RelayUrl>,
    /// The persisted started ordinal currently awaiting a terminal outcome
    /// for each relay.
    attempt_ordinals: BTreeMap<RelayUrl, u64>,
    /// Exact rebuildable projection of this intent's persisted lane rows.
    /// All mutation results enter through `core::lane_projection`; ordinary
    /// worker-demand calculation never re-reads the store.
    lane_projection: LaneWorkerProjection,
    /// Every relay this intent has EVER durably resolved to — the union of
    /// its committed route revisions, held in memory so the queue rewriter
    /// can diff against it without re-reading the store on every pass.
    ///
    /// This is the whole of re-spawn suppression: re-resolution appends only
    /// what is absent here, so a resolver reporting an already-known relay
    /// collides with an existing `(intent_id, relay)` lane and mints
    /// nothing, and an acked lane is terminal and untouched by any later
    /// resolution.
    durable_routes: BTreeSet<RelayUrl>,
    /// False while resolution still holds provider needs. The queue rewriter
    /// re-executes exactly the intents for which this is false, so a
    /// retired `Auto` costs nothing at every later moment.
    ///
    /// Knowledge exhaustion, never delivery: an intent with every lane acked
    /// and one recipient still unresolved is `false`, and one that is
    /// `true` may have delivered nowhere at all.
    route_complete: bool,
    /// Whether the destination picture has been reported at least once. The
    /// FIRST answer is always news, even when it equals the initial state:
    /// "we have looked and found nothing yet" and "we have not looked yet"
    /// are the same VALUE and a different FACT, and an app told nothing
    /// cannot tell them apart.
    destinations_reported: bool,
    /// LATCHED local-persistence fault, if this write ever hit one. Set on
    /// first observation and never cleared — a later ack does not mean the
    /// disk recovered, and an operator must not lose the signal.
    persistence_fault: Option<String>,
    /// The authors whose route provider this intent's last resolution still
    /// needs. This includes ordinary `Unknown` inputs and, when the complete
    /// answer has zero destinations, settled zero-route inputs that must keep
    /// discovery alive for a later positive replacement. Unioned into the
    /// protocol-neutral needs set; re-derived on every resolution, never
    /// persisted and never recovered.
    route_needs: BTreeSet<PublicKey>,
}

/// A live, EngineCore-owned negentropy reconciliation in progress for
/// `sub_id` (plan §6 E). `filter` is already window-erased (since/until/
/// limit cleared) -- ruling §2: "NEG runs unfloored/unlimited"; recording an
/// attribution snapshot straight off this field is therefore always the
/// correct floor:None/until:None/limited:false snapshot the ruling
/// requires, with no separate bookkeeping to keep in sync.
struct NegSession {
    /// Router-owned semantic subscription this reconciliation repairs.
    plan_sub_id: SubId,
    relay: RelayUrl,
    filter: ConcreteFilter,
    absorbed: BTreeSet<CoverageKey>,
    attribution_send: AttributionSendId,
    started_at: Timestamp,
    reconciler: Reconciler,
}

/// A live candidate REQ has been sent with `limit:0`; no Negentropy work is
/// allowed to begin until this exact candidate's EOSE arrives on the exact
/// current transport generation. The previously-active live sub stays open
/// until that barrier, making replacement overlap safe.
struct PendingNegHandoff {
    probed: ProbedRelay,
    plan_sub_id: SubId,
    live_sub_id: SubId,
    prior_live_sub_id: Option<SubId>,
    filter: ConcreteFilter,
    absorbed: BTreeSet<CoverageKey>,
    started_at: Timestamp,
}

enum TemporaryReq {
    /// Missing ids proven by a completed Negentropy exchange. Coverage for
    /// `neg_sub_id` is deferred until this request's EOSE.
    MissingIds {
        plan_sub_id: SubId,
        neg_sub_id: SubId,
        attribution_send: AttributionSendId,
        completed_at: Timestamp,
    },
    /// Plain unlimited backlog fallback after NEG failure/timeout. Its own
    /// attribution snapshot earns coverage directly at EOSE.
    Backlog { plan_sub_id: SubId },
    /// The live candidate never produced EOSE. A later full-backlog EOSE is
    /// also an ordered proof that the earlier candidate REQ was processed;
    /// only then may the prior live sub be retired.
    BacklogActivatesLive {
        plan_sub_id: SubId,
        live_sub_id: SubId,
        prior_live_sub_id: Option<SubId>,
    },
}

#[derive(Debug)]
struct AuthSessionState {
    epoch: AuthEpoch,
    challenge: String,
    last_created_at: Option<Timestamp>,
    policy_instance: Option<AuthCapabilityInstance>,
    signer_instance: Option<AuthCapabilityInstance>,
    phase: AuthSessionPhase,
}

#[derive(Debug)]
enum AuthSessionPhase {
    AwaitingPolicy {
        token: AuthOpToken,
    },
    AwaitingSignature {
        token: AuthOpToken,
        unsigned: UnsignedEvent,
    },
    AwaitingSend {
        token: AuthOpToken,
        event_id: EventId,
        early_ok: Option<(bool, String)>,
    },
    AwaitingOk {
        event_id: EventId,
    },
    Ready {
        event_id: EventId,
    },
    Denied,
    Error,
}

/// The PURE synchronous reducer (§2 position 1). No I/O, no threads.
pub struct EngineCore<S: EventStore> {
    resolver: ResolverEngine<S>,
    router: Router,
    routing_facts: RoutingFactStore,
    cap: usize,
    /// Per-BRANCH bookkeeping for every live observation branch, keyed by
    /// the resolver handle that owns it.
    handles: HashMap<HandleId, BranchState>,
    /// Per-OBSERVATION delivered projection, keyed by the id every mailbox
    /// and cancellation uses.
    observations: HashMap<ObservationId, ObservationState>,
    next_observation_id: u64,
    histories: HashMap<HistorySessionId, HistoryState>,
    history_by_handle: HashMap<HandleId, HistorySessionId>,
    next_history_id: u64,
    attribution: AttributionState,
    pending_request_evidence: HashMap<(RelaySessionKey, SubId), VecDeque<PendingRequestEvidence>>,
    active_request_evidence: HashMap<u64, ActiveRequestEvidence>,
    /// Exact REQs accepted by a live transport generation. Unlike request
    /// evidence, this survives EOSE because EOSE settles a request without
    /// closing its subscription.
    live_wire_requests: HashMap<(RelaySessionKey, SubId), LiveWireRequest>,
    /// EngineCore's memory of the exact connection generation and SESSION
    /// that currently occupy each pool slot. Disconnects are asynchronous;
    /// the generation prevents a delayed old disconnect from erasing a slot
    /// that has already reopened, and the session key prevents a frame
    /// reported for one access context from ever being read as another's
    /// (#8: both halves of the (handle, session) pair must match exactly).
    slot_to_relay: HashMap<u32, (TransportRelayHandle, RelaySessionKey)>,
    /// Sessions CURRENTLY connected — feeds `AcquisitionEvidence.sources[_]
    /// .status` (`Requesting` iff a member here covers the atom;
    /// `Disconnected` iff it was a member of `ever_connected_relays` but
    /// isn't a member here; `Connecting` otherwise). Additive bookkeeping:
    /// `slot_to_relay`'s own semantics (populated on connect, never cleared on
    /// disconnect) are untouched by this.
    connected_relays: BTreeSet<RelaySessionKey>,
    /// Every session that has connected at least once, ever — distinguishes
    /// `Disconnected` (was connected, dropped) from `Connecting` (never yet
    /// connected) for the same evidence computation.
    ever_connected_relays: BTreeSet<RelaySessionKey>,
    /// The exact connection generation that has completed NIP-42 AUTH for
    /// each PROTECTED session (#8). Public sessions never enter this map. A
    /// fresh generation is never pre-authorized (`on_relay_connected` removes
    /// the entry), and readiness dies with the connection
    /// (`on_relay_disconnected` removes it too) — so "ready" always means
    /// "THIS socket, after THIS socket's AUTH handshake", never an earlier
    /// generation's leftover.
    auth_ready_sessions: HashMap<RelaySessionKey, TransportRelayHandle>,
    /// Newly connected author sessions whose first inbound frame is still
    /// being observed for a proactive AUTH challenge. Unlike sticky
    /// `auth_required_sessions`, this exact-generation gate is released by a
    /// transport's ordered first-read completion when an ordinary relay has
    /// no already-available challenge.
    auth_probe_sessions: HashMap<RelaySessionKey, TransportRelayHandle>,
    /// Exact live sessions for which the relay has actually required AUTH:
    /// an AUTH challenge, auth-required write response, or restricted close.
    /// Merely using a frozen NIP-42 access identity does not populate this
    /// set; ordinary relays are released only after the transport's ordered
    /// first socket read-drain completes without an available challenge.
    auth_required_sessions: BTreeSet<RelaySessionKey>,
    /// Current reducer-owned AUTH epoch for each exact protected session.
    /// Entries are removed on disconnect/reconnect teardown; the separate
    /// monotonic counters below deliberately survive that removal so stale
    /// callbacks can never alias a future generation.
    auth_sessions: HashMap<RelaySessionKey, AuthSessionState>,
    next_auth_epoch: Option<u64>,
    next_auth_operation: Option<u64>,
    /// Persisted ordinary-write rows of reserved kind:22242 discovered at
    /// boot. They remain durably inspectable but never regain reducer
    /// ownership, attempt correlations, or a reattachable live delivery.
    quarantined_auth_receipts: HashMap<ReceiptId, QuarantinedWrite>,
    clock: Timestamp,
    active_pubkey: Option<PublicKey>,
    /// Publish queue (§3.4 / VISION §7 ledger #6/#9). `pending` is keyed by
    /// `ReceiptId` from `Publish` through to the last terminal per-relay
    /// status; `event_to_receipt` lets an inbound `OK` frame (keyed by
    /// `EventId` on the wire) find its receipt.
    pending: HashMap<ReceiptId, PendingWrite>,
    /// The stalled-obligation census as of the last diagnostics snapshot
    /// this reducer PUSHED for a write-plane reason.
    ///
    /// A change detector for an observer, never a ledger: it holds no retry
    /// state, no history, and no fact that is not re-derivable from
    /// `pending` in one pass. Its only job is to keep an ordinary healthy
    /// publish from rebuilding an engine-global snapshot at every beat of a
    /// lifecycle in which nothing was ever stuck.
    last_stalled_write_census: Vec<(ReceiptId, StalledWriteStage)>,
    event_to_receipts: HashMap<EventId, BTreeSet<ReceiptId>>,
    /// O(1) reverse index of `pending`'s own `intent_id` field (epic #507
    /// finding E5): `receipt_for_intent` used to be a full linear scan of
    /// `pending`, run once per due deadline in
    /// `consume_due_publish_queue_deadlines`. Maintained at every real
    /// `pending.insert`/`pending.remove` (never at `fail_and_compensate`'s
    /// transient remove-then-reinsert, which never changes which intent a
    /// receipt owns). This mirrors `pending` exactly and needs no separate
    /// invalidation story: it is rebuilt from scratch, in step with
    /// `pending`, every `recover_on_boot`.
    intent_receipts: HashMap<IntentId, ReceiptId>,
    /// Relay -> receipts with a lane on that relay (epic #507 finding E5).
    /// A narrowing INDEX only, never a second source of truth: the store's
    /// `PUBLISH_QUEUE_LANES` table stays authoritative (its keys are intent-first,
    /// and `close_terminal_intent` deliberately never deletes a closed
    /// intent's own terminal lane rows -- both `MemoryStore` and `RedbStore`
    /// only drop `PUBLISH_QUEUE_INTENTS`/the deadline indexes there, per that
    /// door's own doc comment: "Receipts and all route/attempt/detail
    /// evidence are retained" -- so a durable relay-scoped secondary table
    /// would still index retained garbage and would need transactional
    /// maintenance across every lane-writing door).
    /// This index instead rides the reducer's own `pending`/`recover_on_boot`
    /// lifecycle: rebuilt deterministically at boot, so there is no cache-
    /// invalidation question distinct from the one `pending` itself already
    /// answers. `wake_relay_lanes` uses this to avoid re-reading every
    /// outstanding write's lanes on every relay connect/disconnect/auth
    /// event -- it only narrows WHICH intents to re-read via
    /// `recover_publish_queue_lanes`, the store read itself remains the truth.
    /// Kept in lockstep with each `PendingWrite::lane_projection.persisted`
    /// set by the one projection door; cleaned by walking that exact set on
    /// a real removal.
    receipts_by_lane_relay: HashMap<RelayUrl, BTreeSet<ReceiptId>>,
    /// Safety valve for `receipts_by_lane_relay` (epic #507 finding E5): set
    /// to true the moment ANY path could have created/learned lanes but the
    /// index could not record them (a `bootstrap_publish_queue_lanes` or
    /// `recover_route_revisions` error during `recover_on_boot`/`on_signed`).
    /// `recover_on_boot` resets it to false at the start of its one-shot,
    /// deterministic rebuild -- the same moment `pending` itself is rebuilt
    /// from scratch -- and a later failure during that same rebuild (or any
    /// post-boot lane-learning call) sets it back to true for the rest of
    /// this process's life; nothing un-degrades it mid-process, on purpose.
    /// While true, `wake_relay_lanes` falls back to the full
    /// `recover_all_lanes` scan unchanged: a missed wakeup permanently wedges
    /// a durable write lane (the worst bug class here -- see the idle-
    /// barrier missed-wakeup fix, d755f39, and #507's own missed-wakeup
    /// finding), so an unprovable index is always treated as untrustworthy
    /// rather than guessed at.
    lane_relay_index_degraded: bool,
    /// A lane-worker projection gap that NO in-process reconciliation can
    /// close: a committed lane fact arrived for an intent this reducer does
    /// not track, or boot could not read the pending set at all. There is
    /// nothing to retry, so this stays latched until the next
    /// `recover_on_boot` rebuilds `pending` from scratch. Unlike
    /// `lane_relay_index_degraded` it never triggers a fallback scan:
    /// [`Self::relay_worker_requirements`] returns `None` and the runtime
    /// retains every existing session.
    lane_projection_unprovable: bool,
    /// Intents whose durable lane bootstrap did not commit, keyed by their
    /// receipt. Each entry is a *retryable* projection gap, and this map is
    /// the only path out of the conservative retention that gap causes:
    /// `tick` re-runs `bootstrap_projected_lanes` at `due`, and a committed
    /// bootstrap removes the entry. Without it a single transient store
    /// failure would pin the intent's relay workers -- and its receipt --
    /// for the rest of the process (#1000), because `uncertain` is cleared
    /// only by a committed lane fact that no other path can ever produce.
    lane_bootstrap_retries: BTreeMap<ReceiptId, LaneBootstrapRetry>,
    /// The negentropy capability-probe cache (plan §6 E).
    prober: Prober,
    /// Latest provenance-bearing NIP-11 advertisement for relays in the
    /// current read plan. Recompile pruning and completion-time plan checks
    /// prevent historical relay churn from becoming a shadow cache. This is
    /// kept separate from `prober`: advertisement is evidence, never proof.
    nip11_information: HashMap<RelayUrl, RelayInformationCapabilityEvidence>,
    /// Router plan id -> exact NIP-01 subscription currently owning the live
    /// tail. NIP-77 candidates use role-derived ids, so an old live selection
    /// can overlap a replacement until the replacement's EOSE.
    active_nip77_live: HashMap<SubId, SubId>,
    /// Monotonic reincarnation counter for every NIP-77 role wire id
    /// ([`nip77_role_sub_id`], #932). ONLY ever increments: it survives
    /// recompiles, `AttributionState::clear_session`, and reconnects
    /// untouched, because a counter that reset would re-mint a string a
    /// straggler EOSE could still be addressed to -- exactly the defect it
    /// exists to close. `u64` at one mint per repair phase is not a
    /// wrap-around this process can reach.
    next_nip77_incarnation: u64,
    /// Candidate live REQs waiting for their exact EOSE barrier.
    pending_neg_handoffs: HashMap<SubId, PendingNegHandoff>,
    /// Live reconciliation sessions keyed by their role-derived NIP-77 id.
    /// NIP-01 REQ ids and NIP-77 ids are separate namespaces by protocol and
    /// distinct values here, so closing one can never close the other.
    neg_sessions: HashMap<SubId, NegSession>,
    /// Every temporary NIP-01 request outside router demand: missing-id
    /// fetches and ordinary unlimited backlog fallbacks. The typed value
    /// determines the exact EOSE consequence; no boolean lifecycle flag.
    pending_backfills: HashMap<SubId, TemporaryReq>,
    /// The diagnostic surface's own counter (M5 plan §1.2 step 1) — events
    /// actually RECEIVED, per SESSION per kind. Bumped in the
    /// `RelayMessage::Event` arms of `on_relay_frame`/`on_relay_frames`;
    /// read (never mutated) by `diagnostics_snapshot`. This is the one datum
    /// `nmp-router`'s `Diagnostics` cannot see on its own — it never
    /// observes inbound frames, only what was compiled/sent. Wire-observed
    /// counts retain the exact physical session (#8) instead of copying one
    /// URL aggregate into every access-context row.
    events_by_session_kind: HashMap<RelaySessionKey, BTreeMap<u16, u64>>,
    /// Next transport-native [`AttemptCorrelation`] to mint (issue #93).
    /// Purely volatile/in-process — never persisted, never restart-durable
    /// (the plan's own words: "no persistence migration" for this unit).
    /// Checked, typed exhaustion.
    next_attempt_correlation: Option<u64>,
    /// `AttemptCorrelation` -> which receipt/relay it was minted for. Engine-
    /// owned bookkeeping only; transport never needs to understand this
    /// mapping, only echo the correlation back unchanged. An entry is
    /// removed the instant its one-and-only `HandoffResult` arrives — see
    /// `Self::on_event_handoff`.
    attempt_correlations: HashMap<AttemptCorrelation, AttemptCorrelationTarget>,
    /// The provenance-aware relay admission policy for DISCOVERED relays
    /// (issue #121). Protocol components consult it before replacing neutral
    /// author routes.
    /// Defaults to the secure policy (reject every discovered private/
    /// loopback/onion host); production threads the operator's opt-in local
    /// allowlist via [`Self::with_relay_admission`].
    admission: RelayAdmissionPolicy,
    /// Every public key this engine can currently act as: the active account
    /// plus every attached signing capability. This is what "own" means in
    /// provenance-aware admission (#1251), and it is deliberately a SET rather
    /// than the single active account, because `Identity::Explicit` publishes
    /// as a held key without making it active.
    ///
    /// The grant it produces is always keyed to the exact author whose list is
    /// being read, so heeding one held key's own relay list can never widen
    /// what a write signing as a different key is allowed to reach.
    attached_signers: BTreeSet<PublicKey>,
    /// Relays some trusted declaration named, in the exact spelling the
    /// declaration used. It answers ONE question, at the socket boundary:
    /// may this dial reach a local address? Routing has already decided that
    /// nothing untrusted gets here, so this set never widens what is routable
    /// -- it only stops the dial guard from refusing what routing admitted,
    /// which would leave two owners disagreeing about one provenance answer.
    ///
    /// Grants are added, never removed. A relay dropped from a trusted
    /// declaration stops being routed to immediately, so a stale grant names a
    /// destination nothing can reach; revoking it would buy nothing and cost a
    /// reference count over every declaration site.
    heeded_relays: BTreeSet<RelayUrl>,
    /// Monotonic count of discovered route rejections by `admission` before
    /// they could become router candidates (issues #121/#11).
    /// Selector-projected facts count once when a rejected
    /// `(selection, evidence)` first enters current demand, not again on an
    /// unchanged recompile. Surfaced in
    /// [`DiagnosticsSnapshot::discovered_private_relays_rejected`]; the
    /// separate worker-exhaustion cap count lives in the pool
    /// (`nmp_transport::Pool::admission_rejections`) and is folded in by the
    /// runtime.
    discovered_private_relays_rejected: u64,
    /// Rejected selector-projected routing facts present at the previous
    /// recompile. Diffing this set prevents an unchanged demand from
    /// inflating the monotonic rejection counter on every reducer pass.
    rejected_projected_evidence: BTreeSet<(DescriptorHash, RoutingEvidence)>,
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
    /// Runtime relay-worker open failures keyed by their exact current owner.
    /// Entries are pruned whenever demand/write ownership changes and cleared
    /// by a successful connection for that session.
    relay_open_failures: BTreeMap<RelaySessionKey, String>,
    /// Transport health/verifier degradation from a live worker. Kept
    /// separate from open failures so clearing one recovered session cannot
    /// erase an independent transport-health fact.
    transport_degraded: Option<String>,
    /// A failed durable-lane deadline transition is removed from the armed
    /// deadline set until another real engine message retries the reducer.
    /// This prevents a persistent I/O error from becoming recv_timeout(0)
    /// busy-spin while retaining the due row durably for recovery.
    retry_scheduler_blocked: bool,
    /// The attempt ceiling (#1031), from
    /// [`EngineConfig::max_publish_attempts`](crate::EngineConfig). Counts
    /// observations, never wall-clock.
    max_publish_attempts: u64,
    /// Opt-in work counters for lifecycle attribution. Ordinary production
    /// builds pay no field or increment cost.
    #[cfg(any(test, feature = "bench-instrumentation"))]
    projection_store_queries: Cell<u64>,
    #[cfg(any(test, feature = "bench-instrumentation"))]
    router_compiles: Cell<u64>,
    #[cfg(any(test, feature = "bench-instrumentation"))]
    history_store_queries: Cell<u64>,
    #[cfg(test)]
    history_rows_examined: Cell<u64>,
    #[cfg(test)]
    history_affected_row_queries: Cell<u64>,
}

/// What one `AttemptCorrelation` (issue #93) resolves back to in this
/// reducer's own bookkeeping.
struct AttemptCorrelationTarget {
    receipt: ReceiptId,
    /// The write session this attempt rides: the lane's identity-scoped
    /// authenticated session (`Nip42(signing pubkey)`, #8 U2) — an OK is
    /// only ever trusted from the exact session the write published on.
    session: RelaySessionKey,
    /// Durable/AtMostOnce correlations identify the exact persisted lane
    /// ordinal. Ephemeral correlations have no delivery row.
    lane: Option<(IntentId, u64)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AttemptCorrelationExhausted;

impl<S: EventStore> EngineCore<S> {
    pub fn new(store: S, cap: usize) -> Self {
        Self::new_with_routing_facts(store, RoutingFactStore::default(), cap)
    }

    /// Construct a headless reducer over a static fact snapshot.
    ///
    /// This exists for deterministic falsifiers. Production assembly owns
    /// the private mutable fact store and uses [`Self::new`].
    #[doc(hidden)]
    pub fn new_with_fixture_routing_facts(
        store: S,
        facts: nmp_router::FixtureRoutingFacts,
        cap: usize,
    ) -> Self {
        Self::new_with_routing_facts(store, RoutingFactStore::from_fixture(facts), cap)
    }

    pub(crate) fn new_with_routing_facts(
        store: S,
        routing_facts: RoutingFactStore,
        cap: usize,
    ) -> Self {
        // The operator's own lanes are a trusted declaration, so the socket
        // boundary must not refuse what routing already heeds (#1251). An app
        // relay list naming `localhost` is the operator describing their own
        // network, and needs no second opt-in to be dialed.
        let heeded_relays = routing_facts
            .operator_app_relays()
            .into_iter()
            .chain(routing_facts.operator_fallback_relays())
            .collect();
        Self {
            resolver: ResolverEngine::new(store),
            router: Router::new(RuleRegistry::default_widen_only()),
            routing_facts,
            cap,
            handles: HashMap::new(),
            observations: HashMap::new(),
            next_observation_id: 0,
            histories: HashMap::new(),
            history_by_handle: HashMap::new(),
            next_history_id: 1,
            attribution: AttributionState::new(),
            pending_request_evidence: HashMap::new(),
            active_request_evidence: HashMap::new(),
            live_wire_requests: HashMap::new(),
            slot_to_relay: HashMap::new(),
            connected_relays: BTreeSet::new(),
            ever_connected_relays: BTreeSet::new(),
            auth_ready_sessions: HashMap::new(),
            auth_probe_sessions: HashMap::new(),
            auth_required_sessions: BTreeSet::new(),
            auth_sessions: HashMap::new(),
            next_auth_epoch: Some(1),
            next_auth_operation: Some(1),
            quarantined_auth_receipts: HashMap::new(),
            clock: Timestamp::from(0u64),
            active_pubkey: None,
            pending: HashMap::new(),
            last_stalled_write_census: Vec::new(),
            event_to_receipts: HashMap::new(),
            intent_receipts: HashMap::new(),
            receipts_by_lane_relay: HashMap::new(),
            lane_relay_index_degraded: false,
            lane_projection_unprovable: false,
            lane_bootstrap_retries: BTreeMap::new(),
            prober: Prober::new(),
            nip11_information: HashMap::new(),
            active_nip77_live: HashMap::new(),
            next_nip77_incarnation: 0,
            pending_neg_handoffs: HashMap::new(),
            neg_sessions: HashMap::new(),
            pending_backfills: HashMap::new(),
            events_by_session_kind: HashMap::new(),
            next_attempt_correlation: Some(0),
            attempt_correlations: HashMap::new(),
            admission: RelayAdmissionPolicy::default(),
            attached_signers: BTreeSet::new(),
            heeded_relays,
            discovered_private_relays_rejected: 0,
            rejected_projected_evidence: BTreeSet::new(),
            store_degraded: None,
            relay_open_failures: BTreeMap::new(),
            transport_degraded: None,
            retry_scheduler_blocked: false,
            max_publish_attempts: crate::config::DEFAULT_MAX_PUBLISH_ATTEMPTS,
            #[cfg(any(test, feature = "bench-instrumentation"))]
            projection_store_queries: Cell::new(0),
            #[cfg(any(test, feature = "bench-instrumentation"))]
            router_compiles: Cell::new(0),
            #[cfg(any(test, feature = "bench-instrumentation"))]
            history_store_queries: Cell::new(0),
            #[cfg(test)]
            history_rows_examined: Cell::new(0),
            #[cfg(test)]
            history_affected_row_queries: Cell::new(0),
        }
    }

    /// The sole neutral author-route mutation door. Replacement and the
    /// resulting Auto-write wake happen in one reducer turn.
    #[allow(dead_code)]
    pub(crate) fn replace_author_routes(
        &mut self,
        author: PublicKey,
        replacement: AuthorRouteReplacement,
        effects: &mut Vec<Effect>,
    ) {
        let before = self.routing_facts.author_routes(&author);
        if self.is_own_identity(&author) {
            if let AuthorRouteReplacement::Present(routes) = &replacement {
                let declared = routes
                    .outbound()
                    .iter()
                    .chain(routes.inbound())
                    .cloned()
                    .collect::<Vec<_>>();
                self.heeded_relays.extend(declared);
            }
        }
        self.routing_facts.writer().replace(author, replacement);
        if self.routing_facts.author_routes(&author) != before {
            self.recompile(effects);
            self.rewrite_open_routes(effects);
        }
    }

    /// Whether one relay may be used given whose declaration named it, counting a refusal
    /// exactly once for diagnostics.
    #[allow(dead_code)]
    pub(crate) fn admits_relay(
        &mut self,
        relay: &RelayUrl,
        declarer: Declarer,
    ) -> Result<(), RelayRefusal> {
        let outcome = self.admission.admits(relay, declarer);
        if outcome.is_err() {
            self.discovered_private_relays_rejected =
                self.discovered_private_relays_rejected.saturating_add(1);
        }
        outcome
    }

    /// The current neutral author-route fact, including what admission
    /// refused.
    ///
    /// Test-only, and deliberately so: production readers of this fact are
    /// the router (which wants the routable sets) and `exhausted_source`
    /// (which wants the refusals to explain an empty one), and both already
    /// hold `routing_facts`. A second public read door with no production
    /// caller would be a surface nobody needs.
    #[cfg(test)]
    pub(crate) fn author_routes(&self, author: &PublicKey) -> AuthorRouteState {
        self.routing_facts.author_routes(author)
    }

    /// Whether `author` is an identity this engine can act as, and therefore
    /// whether a relay list signed by that key is our own declaration.
    ///
    /// Authorship, not arrival, is the test: a list that reached us by
    /// discovery from a stranger's relay is still ours if we hold the key that
    /// signed it. Signed out with nothing attached, this is false for
    /// everyone, so only the operator tier grants anything.
    #[allow(dead_code)]
    pub(crate) fn is_own_identity(&self, author: &PublicKey) -> bool {
        self.active_pubkey.as_ref() == Some(author) || self.attached_signers.contains(author)
    }

    /// Classify one author's relay list by whose declaration it is.
    #[allow(dead_code)]
    pub(crate) fn relay_list_declarer(&self, author: &PublicKey) -> Declarer {
        if self.is_own_identity(author) {
            Declarer::Ourselves
        } else {
            Declarer::SomeoneElse
        }
    }

    /// Record that a trusted declaration named these relays, so the socket
    /// boundary gives the same provenance answer routing already gave.
    pub(crate) fn heed_relays(&mut self, relays: impl IntoIterator<Item = RelayUrl>) {
        self.heeded_relays.extend(relays);
    }

    /// The provenance answer for one relay at the moment a socket is opened.
    ///
    /// The socket boundary asks the narrower question routing already
    /// answered — may this dial reach a local address? — so it consults the
    /// grants trusted declarations left behind rather than re-deriving
    /// admission from the address, which is how the two layers used to
    /// disagree about one relay.
    pub(crate) fn dial_declarer(&self, relay: &RelayUrl) -> Declarer {
        if self.heeded_relays.contains(relay) {
            Declarer::Ourselves
        } else {
            Declarer::SomeoneElse
        }
    }

    /// Thread the operator's discovered-relay admission policy through
    /// construction (issue #121). Chained onto [`Self::new`] by the runtime
    /// (`engine_loop`); left at the secure default (reject every discovered
    /// private/loopback/onion host) everywhere else, so every test and every
    /// caller that does not opt local hosts in is fail-closed by default.
    /// Set the per-relay attempt ceiling (#1031). Zero is refused into the
    /// finite default: a ceiling of zero would give up before ever trying,
    /// which is a verdict without a single observation behind it.
    #[must_use]
    pub fn with_max_publish_attempts(mut self, max_publish_attempts: u64) -> Self {
        self.max_publish_attempts = if max_publish_attempts == 0 {
            crate::config::DEFAULT_MAX_PUBLISH_ATTEMPTS
        } else {
            max_publish_attempts
        };
        self
    }

    #[must_use]
    pub fn with_relay_admission(mut self, admission: RelayAdmissionPolicy) -> Self {
        self.admission = admission;
        self
    }

    /// Exact relay-SESSION worker demand owned by the reducer right now:
    /// current read-plan sessions plus every nonterminal write lane and every
    /// correlated ephemeral handoff (both as their identity-scoped
    /// `Nip42(signing pubkey)` sessions — #8: a write never rides the Public
    /// read session). The runtime uses this set to release obsolete pool
    /// workers before dispatching replacement wire work, so a finite cap
    /// bounds live work without turning historical read connections into
    /// permanent slot owners.
    ///
    /// This is a pure reducer projection: durable lane reads happen only at
    /// bootstrap/recovery and mutation boundaries, never while reconciling
    /// ordinary worker ownership.
    /// Whether the reducer can prove its lane-worker projection is a
    /// conservative superset of durable nonterminal lanes.
    ///
    /// Derived rather than latched, so every gap that CAN be reconciled
    /// re-enables exact worker reconciliation the moment it is (#1000). A
    /// bootstrap gap whose route candidates are known is already covered by
    /// `LaneWorkerProjection::uncertain` and keeps the projection available;
    /// one whose candidates are unknown has nothing to mark, so it must fall
    /// back to retaining everything until its retry commits.
    fn lane_worker_projection_available(&self) -> bool {
        !self.lane_projection_unprovable
            && self
                .lane_bootstrap_retries
                .values()
                .all(LaneBootstrapRetry::covers_retention)
    }

    pub(crate) fn relay_worker_requirements(&self) -> Option<RelayWorkerRequirements> {
        if !self.lane_worker_projection_available() {
            return None;
        }
        let writes = self.write_relay_workers();
        let mut all: BTreeSet<RelaySessionKey> = self.router.plan().reqs.keys().cloned().collect();
        all.extend(writes.iter().cloned());
        Some(RelayWorkerRequirements { all, writes })
    }

    /// Exact physical sessions owned by nonterminal write work, excluding
    /// read-plan ownership. This builds the `writes` subset in
    /// [`Self::relay_worker_requirements`] so the runtime can give a durable
    /// obligation bounded same-relay time-sharing priority (#598) without
    /// accidentally giving a long-lived protected read the same authority.
    ///
    fn write_relay_workers(&self) -> BTreeSet<RelaySessionKey> {
        let mut required: BTreeSet<RelaySessionKey> = self
            .attempt_correlations
            .values()
            .map(|target| target.session.clone())
            .collect();

        for pending in self.pending.values() {
            let access = AccessContext::Nip42(pending.signing_pubkey);
            required.extend(
                pending
                    .pending_relays
                    .iter()
                    .chain(&pending.unstarted_relays)
                    .chain(&pending.route_blocked_relays)
                    .chain(pending.lane_projection.required_relays())
                    .cloned()
                    .map(|relay| RelaySessionKey::new(relay, access)),
            );
        }

        required
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
        self.wire_demand()
    }

    #[cfg(test)]
    pub(crate) fn observation_ownership_census(&self) -> CoreObservationOwnershipCensus {
        CoreObservationOwnershipCensus {
            handles: self.handles.len(),
            histories: self.histories.len(),
            history_handles: self.history_by_handle.len(),
            resolver_nodes: self.resolver.graph_snapshot().nodes.len(),
            demand_atoms: self.active_demand().len(),
            planned_sessions: self.router.plan().reqs.len(),
            pending_execution_owners: self.pending_request_evidence.len(),
            active_execution_owners: self.active_request_evidence.len(),
            live_wire_owners: self.live_wire_requests.len(),
        }
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
    ) -> Result<Option<nmp_store::CoverageInterval>, PersistenceError> {
        self.resolver
            .store()
            .get_coverage(nmp_store::coverage_key(atom), relay)
    }

    /// The planning-relevant projection of one relay's retained NIP-11
    /// evidence: exactly the pair [`Self::compile_budget`] reads, and
    /// nothing else. Comparing this across a document resolution is what
    /// tells a budget change apart from ordinary document churn.
    fn advertised_planning_limits(&self, relay: &RelayUrl) -> (Option<u64>, Option<u64>) {
        self.nip11_information
            .get(relay)
            .map_or((None, None), |information| {
                (information.max_subscriptions, information.max_subid_length)
            })
    }

    /// Every bound the next `Router::compile` plans within: the operator's
    /// whole-demand relay ceiling, plus whatever each relay in the current
    /// read plan advertised about itself in NIP-11 (#931).
    ///
    /// The advertisements come from `nip11_information`, which `recompile`
    /// already prunes against the still-planned set and
    /// `on_relay_information_resolved` already replaces or REMOVES on every
    /// refresh — so the budget follows a re-published document with no
    /// second cache to age out, and a relay that stops advertising becomes
    /// unbudgeted again rather than keeping a number it has withdrawn.
    ///
    /// A refreshing document is exactly why nothing here may feed identity:
    /// wire ids are allocated tokens, and a `max_subid_length` that moved an
    /// established id would be identity instability
    /// (`docs/internals/subscriptions/identity-grouping-and-limits.md` §6).
    fn compile_budget(&self) -> CompileBudget {
        CompileBudget::with_relay_cap(self.cap).advertising_all(self.nip11_information.iter().map(
            |(relay, information)| {
                (
                    relay.clone(),
                    AdvertisedRelayLimits {
                        max_subscriptions: information
                            .max_subscriptions
                            .map(|value| usize::try_from(value).unwrap_or(usize::MAX)),
                        max_subid_length: information
                            .max_subid_length
                            .map(|value| usize::try_from(value).unwrap_or(usize::MAX)),
                    },
                )
            },
        ))
    }

    /// The engine-global diagnostics projection (M5 plan §1.2 step 2) — "the
    /// acceptance test made visible": combines `nmp_router::Router::
    /// diagnostics()` (per-session wire-sub count, exact filters, lane
    /// counts, reverse coverage) with this reducer's own `events_by_session_
    /// kind` counter and per-(relay, filter) coverage read via
    /// `Self::get_coverage`. Pure and read-only — never influences
    /// routing/delivery; every number here is real state this reducer
    /// already tracks for other reasons, never fabricated/estimated.
    pub fn diagnostics_snapshot(&self) -> DiagnosticsSnapshot {
        let mut snapshot = diagnostics::build(
            self.router.diagnostics(),
            self.router.plan(),
            &self.events_by_session_kind,
            self.discovered_private_relays_rejected,
            |relay, key| self.resolver.store().get_coverage(key, relay),
        );
        // Surface the read-only degrade signal (issue #122) if an ingest/read
        // door has failed — the one persistence-health fact `build` cannot
        // see on its own. The latched engine-wide error is the first one and
        // therefore wins; when the reducer holds none, whatever `build`
        // already recorded stands, because a coverage read that failed while
        // building THIS snapshot is the same fact (#763). Overwriting it
        // unconditionally would present a snapshot whose coverage entries are
        // empty because the store could not be read as a healthy store with
        // nothing proven.
        if self.store_degraded.is_some() {
            snapshot.store_degraded = self.store_degraded.clone();
        }
        snapshot.transport_degraded = self
            .relay_open_failures
            .iter()
            .next()
            .map(|(session, reason)| format!("{}: {reason}", session.relay))
            .or_else(|| self.transport_degraded.clone());
        let mut auth_sessions = BTreeMap::new();
        for (handle, session) in self.slot_to_relay.values() {
            if session.access == AccessContext::Public || !self.connected_relays.contains(session) {
                continue;
            }
            auth_sessions.insert(
                session.clone(),
                AuthDiagnosticsSnapshot {
                    relay: session.relay.clone(),
                    access: session.access,
                    transport_slot: handle.slot,
                    transport_generation: handle.generation,
                    epoch_sequence: None,
                    challenge_hash: None,
                    phase: AuthDiagnosticsPhase::AwaitingChallenge,
                    policy_bound: false,
                    signer_bound: false,
                    auth_event_id: None,
                    send_handoff_accepted: false,
                    relay_ok_accepted: false,
                },
            );
        }
        for (session, state) in &self.auth_sessions {
            let (phase, auth_event_id, send_handoff_accepted, relay_ok_accepted) =
                match &state.phase {
                    AuthSessionPhase::AwaitingPolicy { .. } => {
                        (AuthDiagnosticsPhase::AwaitingPolicy, None, false, false)
                    }
                    AuthSessionPhase::AwaitingSignature { .. } => {
                        (AuthDiagnosticsPhase::AwaitingSignature, None, false, false)
                    }
                    AuthSessionPhase::AwaitingSend { event_id, .. } => (
                        AuthDiagnosticsPhase::AwaitingSend,
                        Some(*event_id),
                        false,
                        false,
                    ),
                    AuthSessionPhase::AwaitingOk { event_id } => (
                        AuthDiagnosticsPhase::AwaitingRelayAck,
                        Some(*event_id),
                        true,
                        false,
                    ),
                    AuthSessionPhase::Ready { event_id } => {
                        (AuthDiagnosticsPhase::Ready, Some(*event_id), true, true)
                    }
                    AuthSessionPhase::Denied => (AuthDiagnosticsPhase::Denied, None, false, false),
                    AuthSessionPhase::Error => (AuthDiagnosticsPhase::Error, None, false, false),
                };
            auth_sessions.insert(
                session.clone(),
                AuthDiagnosticsSnapshot {
                    relay: session.relay.clone(),
                    access: session.access,
                    transport_slot: state.epoch.handle.slot,
                    transport_generation: state.epoch.handle.generation,
                    epoch_sequence: Some(state.epoch.sequence),
                    challenge_hash: (!state.challenge.is_empty()).then(|| {
                        blake3::hash(state.challenge.as_bytes())
                            .to_hex()
                            .to_string()
                    }),
                    phase,
                    policy_bound: state.policy_instance.is_some(),
                    signer_bound: state.signer_instance.is_some(),
                    auth_event_id,
                    send_handoff_accepted,
                    relay_ok_accepted,
                },
            );
        }
        snapshot.auth_sessions = auth_sessions.into_values().collect();
        let (stalled_writes, stalled_write_totals) = self.stalled_write_projection();
        snapshot.stalled_writes = stalled_writes;
        snapshot.stalled_write_totals = stalled_write_totals;
        for relay in &mut snapshot.relays {
            // NIP-11 advertisement and the NIP-77 behavioral probe are both
            // PUBLIC-session evidence (#8): the one-shot HTTP document and
            // the probe run outside/over the unauthenticated session, so a
            // protected session's row must never inherit them — its
            // capability facts stay honestly "unknown".
            if relay.access != AccessContext::Public {
                continue;
            }
            if let Some(information) = self.nip11_information.get(&relay.relay) {
                relay.nip11_supported_nips = information.supported_nips.clone();
                relay.nip11_document_revision = Some(information.document_revision.clone());
                relay.nip11_freshness = Some(if self.clock.as_secs() < information.fresh_until {
                    "fresh"
                } else {
                    "stale"
                });
                relay.nip11_last_error = information.last_error.as_ref().map(ToString::to_string);
            }
            relay.nip77_advertisement = match relay
                .nip11_supported_nips
                .as_ref()
                .map(|nips| nips.contains(&77))
            {
                Some(true) => "advertised_supported",
                Some(false) => "advertised_unsupported",
                None => "unknown",
            };
            relay.nip77_behavior = match self.prober.state(&relay.relay) {
                crate::negentropy::ProbeState::Unknown => "unknown",
                crate::negentropy::ProbeState::Probing => "probing",
                crate::negentropy::ProbeState::Supported => "behaviorally_proven",
                crate::negentropy::ProbeState::Unsupported => "behaviorally_rejected",
            };
            relay.nip77_handoff = if self.pending_backfills.iter().any(|(sub_id, request)| {
                sub_id.0 == relay.relay
                    && matches!(
                        request,
                        TemporaryReq::Backlog { .. } | TemporaryReq::BacklogActivatesLive { .. }
                    )
            }) {
                "fallback_backlog"
            } else if self.pending_backfills.iter().any(|(sub_id, request)| {
                sub_id.0 == relay.relay && matches!(request, TemporaryReq::MissingIds { .. })
            }) {
                "backfilling"
            } else if self
                .neg_sessions
                .values()
                .any(|session| session.relay == relay.relay)
            {
                "reconciling"
            } else if self
                .pending_neg_handoffs
                .keys()
                .any(|sub_id| sub_id.0 == relay.relay)
            {
                "awaiting_live_eose"
            } else if self
                .active_nip77_live
                .keys()
                .any(|plan_sub_id| plan_sub_id.0 == relay.relay)
                && self
                    .connected_relays
                    .contains(&RelaySessionKey::public(relay.relay.clone()))
            {
                "live"
            } else {
                "none"
            };
        }
        snapshot
    }

    /// The one deadline-maintenance transition: advance clock truth, then
    /// execute NIP-40 expiry
    /// (retraction-and-negative-deltas.md §3.2 — drains `store.expire_due`
    /// and retracts every row past its deadline) and the negentropy
    /// and the negentropy liveness-deadline sweep (plan §6 E, harvest
    /// `nmp-nip77`'s "30s
    /// liveness-deadline REQ fallback"): any reconciliation session open
    /// longer than [`NEG_LIVENESS_DEADLINE_SECS`] against `now` is
    /// abandoned in favor of a plain REQ for the same (unfloored/unlimited)
    /// filter. The same tick first consumes every due durable-lane retry/ACK
    /// deadline through the one delivery scheduler.
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
        self.retry_scheduler_blocked = false;
        // Before the durable deadline sweep: a committed bootstrap mints the
        // very lanes the sweep and `schedule_ready` below then act on, so
        // retrying first lets one tick both close the projection gap and
        // make progress on it.
        effects.extend(self.retry_lane_bootstraps(now));
        // Resolution moment THREE: every intent whose routing is not yet
        // complete is re-executed against the directory as it stands NOW.
        // This is the safety net behind moment four's latency path -- it
        // needs no wiring to whatever taught the directory something, and
        // because resolution is diff-and-append it costs an empty diff when
        // nothing was learned.
        self.rewrite_open_routes(&mut effects);
        effects.extend(self.consume_due_publish_queue_deadlines(now));

        // NIP-40 expiry (retraction-and-negative-deltas.md §3.2). The
        // deadline-armed runtime driver above dispatches this tick at the
        // store's next indexed expiration; this reducer owns the atomic
        // removal and projection reaction.
        // Drain every row whose expiration is due straight through the
        // store's own index (`O(log n + due)`, never a scan), then route
        // the removed rows through the SAME retraction lane a kind:5
        // delete already uses inside `ingest_observed` — `resolver.retract`
        // seeds dirty-marks from `removed` alone, then stable simple handles
        // consume the exact committed removals while demand-changing or
        // complex shapes retain the broad refresh oracle.
        match self.resolver.store_mut().expire_due(now) {
            Ok(expired) if !expired.is_empty() => {
                let removed: Vec<_> = expired.into_iter().map(|se| se.event).collect();
                match self.resolver.retract(removed) {
                    Ok(committed) => {
                        self.apply_committed_mutation(committed, &mut effects);
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
        let stale_handoffs: Vec<SubId> = self
            .pending_neg_handoffs
            .iter()
            .filter(|(_, handoff)| now >= handoff.started_at + NEG_LIVENESS_DEADLINE_SECS)
            .map(|(id, _)| id.clone())
            .collect();
        for live_sub_id in stale_handoffs {
            if let Some(handoff) = self.pending_neg_handoffs.remove(&live_sub_id) {
                self.handoff_fallback_to_req(handoff, &mut effects);
            }
        }

        let stale_neg: Vec<SubId> = self
            .neg_sessions
            .iter()
            .filter(|(_, s)| now >= s.started_at + NEG_LIVENESS_DEADLINE_SECS)
            .map(|(id, _)| id.clone())
            .collect();
        for sub_id in stale_neg {
            if let Some(session) = self.neg_sessions.remove(&sub_id) {
                self.neg_session_fallback_to_req(sub_id, session, &mut effects);
            }
        }

        effects
    }

    /// Advance reducer wall-clock truth without executing any deadline work.
    /// Runtime calls this only for transitions whose facts are stamped at the
    /// instant they arrive; due expiry/retry/liveness work remains exclusively
    /// owned by [`Self::tick`] and [`Self::next_deadline`].
    pub(crate) fn advance_clock(&mut self, now: Timestamp) {
        self.clock = now;
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
    ///
    /// Two of the four terms are durable and therefore fallible, and this
    /// door hands both failures straight to its caller rather than folding
    /// them into `None` (#763). The distinction is the whole point: `Ok(None)`
    /// tells the driver to park on a plain `recv()` forever, which is correct
    /// only when there is genuinely nothing to wake up for. A read that could
    /// not answer is not that, and the delivery term reaching here as
    /// `.ok().flatten()` is how a durable, due obligation could stop being
    /// scheduled with nothing recording why. `runtime::engine_loop` degrades
    /// the store on `Err`, which is the #122 fact an app already reads.
    pub fn next_deadline(&self) -> Result<Option<Timestamp>, PersistenceError> {
        let expiry = self.resolver.store().next_expiration()?;
        let neg_liveness = self
            .neg_sessions
            .values()
            .map(|session| session.started_at + NEG_LIVENESS_DEADLINE_SECS)
            .chain(
                self.pending_neg_handoffs
                    .values()
                    .map(|handoff| handoff.started_at + NEG_LIVENESS_DEADLINE_SECS),
            )
            .min();
        // A persistence failure already latched by the write plane suppresses
        // this term until real work arrives (`handle` clears the flag), which
        // is a recorded decision rather than an erased read. The read itself
        // still propagates.
        let delivery = match (!self.retry_scheduler_blocked)
            .then(|| self.resolver.store().next_publish_queue_deadline())
        {
            Some(read) => read?,
            None => None,
        };
        // Lane-bootstrap retries carry their own capped backoff, so unlike
        // the delivery deadline they are NOT suppressed by
        // `retry_scheduler_blocked`: a failed bootstrap has no durable
        // deadline row to rearm it, and suppressing it here would leave the
        // intent's conservative retention with no way out (#1000).
        let bootstrap = self
            .lane_bootstrap_retries
            .values()
            .map(|retry| retry.due)
            .min();
        Ok([expiry, neg_liveness, delivery, bootstrap]
            .into_iter()
            .flatten()
            .min())
    }

    pub fn handle(&mut self, msg: EngineMsg) -> Vec<Effect> {
        // A prior persistence failure suppresses a due delivery deadline only
        // until real work arrives. Re-expose it after this message so the
        // runtime immediately drives a fresh Tick instead of either spinning
        // on the failed transition or suppressing retry forever.
        self.retry_scheduler_blocked = false;
        let mut effects = match msg {
            EngineMsg::Subscribe(query) => self.on_subscribe(query),
            EngineMsg::FlushWireAdmission(now) => self.flush_wire_admission(now),
            EngineMsg::Unsubscribe(id) => self.on_unsubscribe(id),
            EngineMsg::SubscribeHistory(query) => self.on_subscribe_history(query),
            EngineMsg::RequestRows(id, at_least) => self.on_request_rows(id, at_least),
            EngineMsg::CommitHistoryLoad(id) => self.on_commit_history_load(id),
            EngineMsg::RollbackHistoryLoad(id) => self.on_rollback_history_load(id),
            EngineMsg::UnsubscribeHistory(id) => self.on_unsubscribe_history(id),
            EngineMsg::SetActivePubkey(pk) => self.on_set_active_pubkey(pk),
            EngineMsg::Publish(intent) => self.on_publish(intent),
            EngineMsg::RelayConnected(handle, session) => self.on_relay_connected(handle, session),
            EngineMsg::AuthProbeReleased(handle, session) => {
                self.on_auth_probe_released(handle, session)
            }
            EngineMsg::RelayInformationResolved(url, information) => {
                self.on_relay_information_resolved(url, information)
            }
            EngineMsg::RelayDisconnected(handle, session, reason) => {
                self.on_relay_disconnected(handle, session, reason)
            }
            EngineMsg::RelayHealth(handle, session, health) => {
                self.on_relay_health(handle, session, health)
            }
            EngineMsg::RelayOpenFailed(session, reason) => {
                if self
                    .relay_worker_requirements()
                    .is_some_and(|required| required.all.contains(&session))
                {
                    self.relay_open_failures.insert(session, reason);
                    vec![Effect::EmitDiagnostics(self.diagnostics_snapshot())]
                } else {
                    Vec::new()
                }
            }
            EngineMsg::RelayFrame(handle, session, frame) => {
                self.on_relay_frame(handle, session, frame)
            }
            EngineMsg::RelayFrames(frames) => self.on_relay_frames(frames),
            EngineMsg::SignerCompleted(id, generation, result) => {
                self.on_signer_completed(id, generation, result)
            }
            EngineMsg::SignerUnavailable(id, generation) => {
                self.on_signer_unavailable(id, generation)
            }
            EngineMsg::SignerAttached(pk) => self.on_signer_attached(pk),
            EngineMsg::AuthPolicyCompleted(token, instance, outcome) => {
                self.on_auth_policy_completed(token, instance, outcome)
            }
            EngineMsg::AuthSignerCompleted(token, instance, outcome) => {
                self.on_auth_signer_completed(token, instance, outcome)
            }
            EngineMsg::AuthCapabilityBound {
                token,
                capability,
                instance,
            } => self.on_auth_capability_bound(token, capability, instance),
            EngineMsg::AuthSendCompleted(completion) => self.on_auth_send_completed(completion),
            EngineMsg::AuthCapabilityInvalidated(pubkey, capability, instance) => {
                self.on_auth_capability_invalidated(pubkey, capability, instance)
            }
            EngineMsg::CancelWrite(id) => self.cancel_write(id).1,
            EngineMsg::EventHandoff(correlation, result) => {
                self.on_event_handoff(correlation, result)
            }
            EngineMsg::Tick(now) => self.tick(now),
        };
        // A write-plane transition can start or end a STALL, and the
        // stalled-write section lives on this same snapshot (#756). Without
        // this, that section would only ever be pushed by unrelated
        // read-plane traffic: an app that published one obligation into an
        // outage and then sat still would be told nothing was stuck, which
        // is exactly the failure the section exists to prevent.
        //
        // Gated on the CENSUS changing rather than on the turn having
        // touched a receipt: an ordinary healthy publish moves through
        // accepted/signed/routed/sent/acked without ever being stuck, and
        // pushing a fresh engine-global snapshot at each of those beats
        // would make every ACK cost a full diagnostics rebuild for no new
        // fact. The census is cheap by construction (no formatted detail, no
        // descriptor) precisely so this guard is cheaper than what it skips.
        if effects
            .iter()
            .any(|effect| matches!(effect, Effect::EmitReceipt(..)))
        {
            let census = self.stalled_write_census();
            if census != self.last_stalled_write_census {
                self.last_stalled_write_census = census;
                effects.push(Effect::EmitDiagnostics(self.diagnostics_snapshot()));
            }
        }
        if self.prune_unowned_relay_state() {
            effects.push(Effect::EmitDiagnostics(self.diagnostics_snapshot()));
        }
        effects
    }

    fn prune_unowned_relay_state(&mut self) -> bool {
        if self.relay_open_failures.is_empty() && self.auth_required_sessions.is_empty() {
            return false;
        }
        let Some(required) = self.relay_worker_requirements() else {
            return false;
        };
        let before = self.relay_open_failures.len();
        self.relay_open_failures
            .retain(|session, _| required.all.contains(session));
        self.auth_required_sessions
            .retain(|session| required.all.contains(session));
        self.relay_open_failures.len() != before
    }

    fn on_relay_health(
        &mut self,
        handle: TransportRelayHandle,
        session: RelaySessionKey,
        health: RelayHealth,
    ) -> Vec<Effect> {
        // Health delivery crosses the off-lock sink and may arrive after the
        // slot has reopened for a different generation OR a different
        // session: accept it only when BOTH halves of the reported
        // (handle, session) pair are exactly the slot's current occupant
        // (#8) — health from a slot never seen connected proves nothing.
        let Some((current, current_session)) = self.slot_to_relay.get(&handle.slot) else {
            return Vec::new();
        };
        if *current != handle || *current_session != session {
            return Vec::new();
        }
        self.transport_degraded = health.last_error.or_else(|| {
            (health.invalid_signature_count > 0).then(|| {
                format!(
                    "relay slot {} rejected {} invalid signature frame(s)",
                    handle.slot, health.invalid_signature_count
                )
            })
        });
        vec![Effect::EmitDiagnostics(self.diagnostics_snapshot())]
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
        let ids: Vec<_> = self.handles.keys().copied().collect();
        for id in ids {
            self.reconcile_observation_resolution(
                id,
                ResolutionCause::ActiveAccountChanged,
                &mut effects,
            );
        }
        self.recompile(&mut effects);
        self.refresh_all_observations(&mut effects);
        self.refresh_all_histories(&mut effects);
        if let Some(pk) = pk {
            // The runtime moves its active signer pointer before delivering
            // this message. Re-arm matching accepted work here as well as
            // on SignerAttached so both ordering cases (activate→attach and
            // attach→activate) converge without polling.
            effects.extend(self.on_signer_attached(pk));
        }
        effects
    }
}

#[cfg(feature = "bench-instrumentation")]
impl EngineCore<nmp_store::RedbStore> {
    /// Reset reducer lifecycle counters independently from Redb's row-work
    /// counters so a benchmark can attribute admission and projection work.
    #[doc(hidden)]
    pub fn bench_reset_lifecycle_work(&self) {
        self.projection_store_queries.set(0);
        self.router_compiles.set(0);
        self.history_store_queries.set(0);
    }

    /// `(ordinary projection reads, router compiles, history projection
    /// reads)` since the last lifecycle reset.
    #[doc(hidden)]
    pub fn bench_lifecycle_work(&self) -> (u64, u64, u64) {
        (
            self.projection_store_queries.get(),
            self.router_compiles.get(),
            self.history_store_queries.get(),
        )
    }

    /// Benchmark-only access to the store work counters used by the
    /// million-row scale proofs. Not an application/store API.
    #[doc(hidden)]
    pub fn bench_reset_query_work(&self) {
        self.resolver.store().reset_query_work();
    }

    #[doc(hidden)]
    pub fn bench_query_work(&self) -> (u64, u64, u64) {
        self.resolver.store().query_work()
    }

    /// Coverage-table point reads are counted separately from event
    /// projection rows because diagnostics and freshness evidence use them.
    #[doc(hidden)]
    pub fn bench_reset_coverage_reads(&self) {
        self.resolver.store().reset_coverage_reads();
    }

    #[doc(hidden)]
    pub fn bench_coverage_reads(&self) -> u64 {
        self.resolver.store().coverage_reads()
    }

    /// Drive the production committed-delta path without constructing a
    /// transport frame; the benchmark already owns verified signed events
    /// and explicit relay observations.
    #[doc(hidden)]
    pub fn bench_ingest_observed(
        &mut self,
        events: Vec<(SignedEvent, RelayObserved)>,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        self.ingest_relay_events(events, &mut effects);
        effects
    }

    /// Exact pre-#195 comparison lane: commit through the same resolver/store
    /// door, then force the old affected-handle full refresh. Restricted to
    /// ordinary benchmark events whose demand/directory shape cannot change.
    #[doc(hidden)]
    pub fn bench_ingest_observed_with_forced_refresh(
        &mut self,
        events: Vec<(SignedEvent, RelayObserved)>,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        for (event, observed) in &events {
            // Benchmark observations carry only a URL; they ride the Public
            // session's counter row, the same session the production frame
            // path would attribute an unauthenticated observation to.
            *self
                .events_by_session_kind
                .entry(RelaySessionKey::public(observed.relay.clone()))
                .or_default()
                .entry(event.kind.as_u16())
                .or_insert(0) += 1;
        }
        let ingest = self
            .resolver
            .ingest_observed_detailed(events)
            .expect("benchmark fixture store commit");
        assert!(
            ingest.committed.delta.is_empty(),
            "benchmark shape changed demand"
        );
        assert!(
            ingest.satisfied_intents.is_empty(),
            "benchmark event unexpectedly satisfied a local intent"
        );
        effects.push(Effect::EmitDiagnostics(self.diagnostics_snapshot()));
        self.refresh_observations_of_branches(ingest.committed.affected_handles, &mut effects);
        effects
    }

    /// Commit a benchmark local write through the real governed
    /// `accept_write`/resolver door, then use the production projection
    /// policy added by #228. Receipt/signing/routing orchestration is outside
    /// the measured mutation seam and deliberately omitted.
    #[doc(hidden)]
    pub fn bench_accept_local(&mut self, accept: AcceptWrite) -> Vec<Effect> {
        let accepted = self
            .resolver
            .accept_local(accept)
            .expect("benchmark local acceptance commit");
        assert!(
            accepted.outcome.journaled_intent_id().is_some(),
            "benchmark local acceptance must be journaled"
        );
        let mut effects = Vec::new();
        self.apply_committed_mutation(accepted.committed, &mut effects);
        effects
    }

    /// Exact pre-#228 comparison for the same local acceptance commit: keep
    /// reactive-demand fallback behavior, but force stable-shape handles
    /// through the former full-refresh projection.
    #[doc(hidden)]
    pub fn bench_accept_local_with_forced_refresh(&mut self, accept: AcceptWrite) -> Vec<Effect> {
        let accepted = self
            .resolver
            .accept_local(accept)
            .expect("benchmark local acceptance commit");
        assert!(
            accepted.outcome.journaled_intent_id().is_some(),
            "benchmark local acceptance must be journaled"
        );
        let CommittedMutationResult {
            delta,
            affected_handles: _,
            row_changes: _,
        } = accepted.committed;
        assert!(delta.is_empty(), "benchmark local write changed demand");
        let mut effects = Vec::new();
        self.recompile(&mut effects);
        self.refresh_all_observations(&mut effects);
        effects
    }

    /// Expire due rows through the production store/retraction/projection
    /// path. The fixture supplies exactly one due row per measured call.
    #[doc(hidden)]
    pub fn bench_expire_due(&mut self, now: Timestamp) -> Vec<Effect> {
        self.bench_expire_due_with_mode(now, false)
    }

    /// Exact pre-#228 expiry comparison: same governed store mutation and
    /// resolver reaction, followed by the former recompile/full refresh.
    #[doc(hidden)]
    pub fn bench_expire_due_with_forced_refresh(&mut self, now: Timestamp) -> Vec<Effect> {
        self.bench_expire_due_with_mode(now, true)
    }

    fn bench_expire_due_with_mode(&mut self, now: Timestamp, force_refresh: bool) -> Vec<Effect> {
        let expired = self
            .resolver
            .store_mut()
            .expire_due(now)
            .expect("benchmark expiry commit");
        assert_eq!(expired.len(), 1, "benchmark owns exactly one due row");
        let removed = expired.into_iter().map(|row| row.event).collect();
        let committed = self
            .resolver
            .retract(removed)
            .expect("benchmark expiry reaction");
        let mut effects = Vec::new();
        if force_refresh {
            let CommittedMutationResult {
                delta,
                affected_handles: _,
                row_changes: _,
            } = committed;
            assert!(delta.is_empty(), "benchmark expiry changed demand");
            self.recompile(&mut effects);
            self.refresh_all_observations(&mut effects);
        } else {
            self.apply_committed_mutation(committed, &mut effects);
        }
        effects
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

#[cfg(test)]
mod history_load_failure_tests;
