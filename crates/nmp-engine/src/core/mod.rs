//! The synchronous reducer and durable-state owner (plan §2 position 1,
//! §3.4). `EngineCore` owns the concrete `RedbStore`, the M1 resolver
//! `Engine`, the M2 `Router`, the write-delivery state, and the
//! coverage-attribution bookkeeping (`attribution.rs`, `evidence.rs`). Its
//! main message-driven surface is:
//!
//! ```ignore
//! impl EngineCore {
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
//! `EngineCore` performs synchronous durable I/O through its `RedbStore`, but
//! spawns no threads, touches no socket, and imposes no runtime. This is the
//! seam that preserves M1/M2's headless property: the whole engine's logic is
//! testable by feeding `EngineMsg`s and asserting `Effect`s against a concrete
//! temporary or persistent store, with zero network (plan §5 tier A).
//!
//! Coverage attribution follows
//! `docs/design/query-demand-and-evidence.md` plus issue #816's
//! request-scoped facts-before-claims contract: send-time snapshots + the
//! FIFO intersection rule live in [`attribution`]; the per-query, per-source
//! acquisition evidence (`rows + compact facts, never a collapsed global
//! verdict` — `docs/design/scoped-evidence-49-12-plan.md`, folding #12 into
//! #49) lives in [`evidence`]. Both are engine-owned — the store
//! (`nmp-store`) only stores whatever interval it is handed.

#[cfg(test)]
mod admission_tests;
mod attribution;
mod author_route_provider;
pub use author_route_provider::{AuthorRouteProvider, AuthorRouteUpdate, ProviderReroot};
#[cfg(test)]
mod auth_core_headless;
mod auth_transport;
#[cfg(test)]
mod auth_transport_tests;
mod coordinate_coverage;
mod diagnostics;
mod evidence;
#[cfg(test)]
mod freshness_snapshot_tests;
#[cfg(test)]
mod handoff_starvation_tests;
mod history;
mod history_lifecycle;
#[cfg(test)]
mod history_lifecycle_tests;
#[cfg(test)]
mod lane_bootstrap_retry_tests;
mod lane_projection;
#[cfg(test)]
mod nip77_metadata_tests;
mod nip77_sessions;
mod observation;
#[cfg(test)]
mod outbox_tests;
mod query;
#[cfg(test)]
mod query_tests;
mod request_attempt;
#[cfg(test)]
mod request_attempt_tests;
mod request_effects;
#[cfg(test)]
mod request_replacement_transition_tests;
mod request_targets;
mod semantic_delivery;
#[cfg(test)]
mod semantic_settlement_falsifier_tests;
#[cfg(test)]
mod transport_tests;
mod wire_ownership;
mod write;
pub use write::{PreparedReplaceableMaterialization, PublishPreparation};
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

/// Only the wire owner names routing-evidence facts in production now; the
/// admission tests still construct them directly through this module's glob.
#[cfg(test)]
use nmp_grammar::RoutingEvidence;
use nmp_grammar::{
    fold_byte, AccessContext, CacheMode, ConcreteFilter, ContextualAtom, DemandDelta, DemandOp,
    DescriptorHash, Freshness, Identity, LiveQuery, RelaySessionKey,
    ReplaceableMaterializerOperation, ReplaceableMaterializerRegistration, SourceAuthority,
    WriteIntent, WritePayload, WriteRouting,
};
use nmp_resolver::{
    CommittedCurrentRow, CommittedMutationResult, CommittedRowChanges, Engine as ResolverEngine,
    HandleId, LocalAcceptResult, QueryHandle, RelayIngestError, SubscribeOutcome,
};
use nmp_router::{
    AdvertisedRelayLimits, AuthorRouteState, AuthorRoutes, CompileBudget, RelayPlan, Router,
    RoutingFacts, RuleRegistry, SubId, WireDelta, WireOp,
};
use nmp_signer::SignerError;
use nmp_store::{
    sentinel_signature, AcceptOutcome, AcceptWrite, AcceptWritePayload, AccessContextId,
    AuthDenial as StoredAuthDenial, AuthDenialSource as StoredAuthDenialSource, CloseIntentOutcome,
    CompensateOutcome, CoverageInterval, CoverageKey, DurabilityOutcome, HandoffEvidence, IntentId,
    IntentSigState, MaterializationCandidate, PendingMaterializationState, PersistenceError,
    PersistenceFault, PromoteOutcome, PromotionTarget, PublishQueueAttemptHandoff,
    PublishQueueAttemptOutcome, PublishQueueDeadlineKind, PublishQueueInFlightPhase,
    PublishQueueLane, PublishQueueLaneKey, PublishQueueLaneState, PublishQueuePostHandoffState,
    PublishQueueReceipt, PublishQueueReceiptPayload, PublishQueueTerminalOutcome,
    PublishQueueTransientCause, QualifiedSource, ReceiptState, RedbStore, RelayObserved,
    RemoveQueueEntryOutcome, ReplaceableMaterializationTarget, ReplayFormatId, ReplayProgramId,
    SemanticAccept, SemanticPlan, SemanticRematerialize, SemanticSourceInstall, SigState,
    SourceEvidence as SemanticSourceEvidence, SourcePlanId, StartingSource,
    StartingSourceRequirement, VerifiedSignature,
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

type AttributedRelayObservation = (
    SignedEvent,
    RelayObserved,
    Option<CommittedObservationCandidate>,
    Option<(RelaySessionKey, String)>,
);

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
pub struct RoutingFactStore {
    authors: BTreeMap<PublicKey, AuthorRouteState>,
    operator_app: Vec<RelayUrl>,
    operator_fallback: Vec<RelayUrl>,
}

impl RoutingFactStore {
    pub fn new(
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

    #[cfg(feature = "unstable-mechanism")]
    pub fn from_fixture(fixture: nmp_router_testkit::FixtureRoutingFacts) -> Self {
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
pub enum AuthorRouteReplacement {
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

use attribution::{AttributionSendId, AttributionState, CompletedAttribution, EventFailureTarget};
use diagnostics::{stalled_write_id, STALLED_WRITE_DETAIL_LIMIT};
pub use diagnostics::{
    AuthDiagnosticsPhase, AuthDiagnosticsSnapshot, DiagnosticsSnapshot, FilterCoverageEntry,
    RelayDiagnosticsSnapshot, StalledWrite, StalledWriteStage, StalledWriteTotals,
};
pub use evidence::{AcquisitionEvidence, AuthPhase, ShortfallFact, SourceEvidence, SourceStatus};
pub use history::{HistoryAdvanceError, HistoryBatch, HistoryQuery, HistorySessionId, WindowLoad};
use history_lifecycle::HistorySessions;
use nip77_sessions::Nip77Sessions;
use observation::{
    ActiveRequestEvidence, LiveWireRequest, ObservationExecutionState, PendingRequestEvidence,
};
pub use observation::{
    ObservationEvidence, ObservationFact, RequestTerminal, ResolutionCause, ResolvedBindingValue,
};
pub use query::Nip77Frame;
pub use request_attempt::{LocalSendRefusal, RequestAttemptId, RequestHandoffOutcome};
use request_attempt::{RequestAttemptPurpose, RequestAttemptState, RequestAttempts, RequestSend};
pub use request_effects::{AttemptedReplay, AttemptedWireDelta};
use request_targets::{ActiveRequestTarget, RequestTargets};
use wire_ownership::{AtomReleased, AtomRetained, WireOwnership};
// `runtime` (C) needs the EXACT same wire subscription-id string
// `attribution.rs` records at send time (`AttributionState::record_send`) so
// that a REQ actually placed on the wire under this string round-trips back
// to the right `SubId` when the relay echoes it in an EOSE — re-derive it or
// drift silently breaks coverage attribution. `pub(crate)` (not a wider
// re-export): this is an internal wire-format detail `core` and `runtime`
// share, never a public contract for callers outside this crate.
pub use attribution::wire_sub_id_string;

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
    pub fn new(receipt_id: ReceiptId) -> Self {
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

/// The reasons `publish()` refuses before taking custody.
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
/// [`Self::NoCurrentAccount`], [`Self::SignatureInvalid`],
/// [`Self::IdentityContradictsSignedAuthor`], [`Self::ReservedKind`],
/// [`Self::EmptyExplicitRoute`], [`Self::AlreadyExpired`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishError {
    /// The runtime has begun its finite cancellation/drain phase and cannot
    /// accept a new write before closing.
    EngineShuttingDown,
    /// The acceptance transaction itself failed. Recording the failure would
    /// need the disk that just refused, so there is no queue entry to fail
    /// into.
    PersistenceFailed { reason: String },
    /// [`Identity::Active`](nmp_grammar::Identity::Active) with no current
    /// account. Nothing is pinned, so nothing may park — and a later login
    /// could sign as the wrong person.
    NoCurrentAccount,
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
    /// The event's own NIP-40 expiration was already at or before the
    /// acceptance timestamp. NMP never takes custody of work that is
    /// impossible to publish usefully.
    AlreadyExpired,
    /// The closed semantic operation or its exact source witness no longer
    /// matched at the atomic acceptance door. Nothing entered custody.
    ReplaceableOperationRefused { reason: String },
}

impl std::fmt::Display for PublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EngineShuttingDown => write!(f, "engine is shutting down"),
            Self::PersistenceFailed { reason } => {
                write!(f, "the write could not be recorded: {reason}")
            }
            Self::NoCurrentAccount => write!(
                f,
                "publishing with the current-account identity requires a current account"
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
            Self::AlreadyExpired => write!(f, "the event was already expired at acceptance"),
            Self::ReplaceableOperationRefused { reason } => {
                write!(f, "the replaceable operation was refused: {reason}")
            }
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
    pub end_cursor: Option<ReceiptReplayCursor>,
    /// The frozen event id of the receipt this page replayed, read from the
    /// same durable record. `Some` exactly when `outcome` is `Attached`: an
    /// absent or unreadable receipt has no identity to report. A
    /// correlation-idempotent republish resolves to an existing obligation
    /// instead of accepting a new one, and this is where its acceptance
    /// answer gets the same event id a first acceptance returns.
    pub frozen_id: Option<EventId>,
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

/// `Row`/`RowSignature`/`RowDelta` moved to `nmp-grammar` (#1707): they are
/// pure value types with no engine/store/router access in their own
/// methods, the read-side counterpart to `WriteIntent`. Re-exported here
/// unchanged so every `core/**` file that reaches them through `use
/// super::*` keeps compiling untouched.
pub use nmp_grammar::{first_verified_source, Row, RowDelta, RowSignature};

/// The two constructors that stayed behind: `RowSignature::from_store`/
/// `Row::from_stored_event` named `nmp_store::SigState` directly, and
/// `nmp-store` already depends on `nmp-grammar`, so moving them down would
/// be a package cycle (`nmp-grammar -> nmp-store -> nmp-grammar`). Both are
/// pure translations expressible through `Row::from_parts` -- no private
/// field access needed -- so they live here as free functions instead of
/// methods, the same shape team-lead's brief prescribed. `Row` itself does
/// not get an inherent impl outside the crate that defines it (there is no
/// way to add one -- Rust's orphan rule forbids it), which is exactly why
/// these are free functions rather than an `impl Row` block reopened here.
fn row_signature_from_store_state(event: &SignedEvent, state: SigState) -> RowSignature {
    match state {
        SigState::Pending => RowSignature::Pending,
        SigState::Signed => RowSignature::Signed(event.sig),
    }
}

pub fn row_from_stored_event(
    event: nostr::Event,
    signature_state: SigState,
    sources: BTreeSet<RelayUrl>,
) -> Row {
    let signature = row_signature_from_store_state(&event, signature_state);
    Row::from_parts(
        event.id,
        event.pubkey,
        event.created_at,
        event.kind,
        event.tags,
        event.content,
        signature,
        sources,
    )
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
/// text, event ids, the current account, or callback arrival order.
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

/// The provenance-bearing subset of a NIP-11 document used by engine
/// capability decisions and diagnostics. It deliberately excludes runtime
/// connection/AUTH state.
///
/// This is reducer INPUT VOCABULARY and it lives here, not in `nmp-nip11`,
/// for the reason that crate exists at all: acquisition is HTTP, and a
/// reducer that named the acquiring crate's types would drag `reqwest` into
/// its own manifest. `runtime` projects a `RelayInformationSnapshot` into
/// this value the same way it projects an author-route provider's answer
/// into an [`AuthorRouteUpdate`]. Nothing below this line knows an HTTP
/// client exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayInformationCapabilityEvidence {
    pub supported_nips: Option<Vec<u16>>,
    /// `limitation.max_subscriptions` — concurrent subscriptions this relay
    /// will hold open on one connection. `None` is "advertised nothing",
    /// which the router reads as UNBUDGETED rather than as any number
    /// (`nmp_router::budget`). Enforced planning input, not presentation.
    pub max_subscriptions: Option<u64>,
    /// `limitation.max_subid_length` — the longest subscription id this
    /// relay accepts. DIAGNOSED only: NMP's wire ids are fixed 64-character
    /// strings, and a relay advertising less rejects every REQ we send. It
    /// must never feed id derivation, because this document refreshes and a
    /// mutable derivation input is identity instability.
    pub max_subid_length: Option<u64>,
    pub document_revision: String,
    /// Absolute Unix-seconds deadline. Diagnostics derives freshness from
    /// the engine clock instead of retaining a read-time label forever.
    pub fresh_until: u64,
    /// Already rendered. The reducer's only use of the last acquisition
    /// failure is the diagnostics string it copies into
    /// `RelayDiagnostics::nip11_last_error`; it never matched a variant.
    /// Carrying `nmp_nip11::RelayInformationError` here instead would be the
    /// one type dependency that puts an HTTP client back in the reducer's
    /// manifest, to buy a `Display` call `runtime` can make itself.
    pub last_error: Option<String>,
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
    /// Runtime could not create a required relay worker, or that live worker
    /// reported a transient failure during its current connection attempt.
    /// Observational only: current demand remains the retry owner while
    /// diagnostics and query-scoped evidence retain the exact failure
    /// instead of silently presenting a merely connecting session forever.
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
pub enum ObservationOpen<Id, Seed> {
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

pub struct RowsSeed {
    pub deltas: Vec<RowDelta>,
    /// Per-BRANCH acquisition evidence in canonical branch order (#1108).
    pub evidence: Vec<AcquisitionEvidence>,
}

/// Deterministic ordinary-withdrawal work counters for scale harnesses.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CoreWithdrawalWork {
    pub handles_detached: u64,
    pub resolver_delta_ops_consumed: u64,
    pub resolver_owner_keys_touched: u64,
    pub resolver_surviving_atoms_examined: u64,
    pub pending_atoms_rebuilt: u64,
    pub evidence_candidates_examined: u64,
    pub routing_evidence_owner_keys_touched: u64,
    pub diagnostic_snapshots_built: u64,
    pub exact_atoms_closed: u64,
    pub request_edges_touched: u64,
    pub plan_request_entries_visited: u64,
    pub requests_closed: u64,
    pub physical_coverage_edges_released: u64,
    pub diagnostic_refreshes: u64,
    pub diagnostic_requests_visited: u64,
    pub nip77_plan_children_touched: u64,
}

/// Deterministic pending-admission work counters for incumbent-isolation
/// scale harnesses.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CoreAdmissionWork {
    pub pending_atoms_rebuilt: u64,
    pub pending_cohort_atoms_reconciled: u64,
    pub attribution_atoms_rebuilt: u64,
    pub evidence_candidates_examined: u64,
    pub request_target_demand_keys_touched: u64,
    pub request_target_candidates_examined: u64,
    pub request_claim_entries_examined: u64,
    pub request_owner_entries_examined: u64,
    pub request_claim_transfer_attempts: u64,
    pub request_claim_transfer_claims_attempted: u64,
    pub request_claim_transfer_commits: u64,
    pub request_claim_transfer_failures: u64,
    pub diagnostic_snapshots_built: u64,
    pub cohort_compiles: u64,
    pub incumbent_active_entries_visited: u64,
    pub incumbent_plan_requests_visited: u64,
    pub incumbent_limited_entries_visited: u64,
    pub incumbent_refusal_entries_visited: u64,
    pub active_entries_appended: u64,
    pub request_edges_appended: u64,
    pub metadata_entries_examined: u64,
}

/// Deterministic candidate-local work performed by opening-time freshness.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CoreFreshnessWork {
    pub candidate_atoms: u64,
    pub incumbent_demand_edges_visited: u64,
    pub plan_request_entries_visited: u64,
    pub coalesce_pair_attempts: u64,
}

/// Exact local ownership retained by the reducer after a lifecycle step.
#[cfg(any(test, feature = "bench-instrumentation"))]
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CoreOwnershipCensus {
    pub observations: usize,
    pub branch_handles: usize,
    pub retained_freshness_source_edges: usize,
    pub request_target_handles: usize,
    pub request_target_demand_keys: usize,
    pub request_target_edges: usize,
    pub request_target_refs: usize,
    pub active_request_target_handles: usize,
    pub active_request_target_demand_keys: usize,
    pub active_request_target_edges: usize,
    pub active_request_target_refs: usize,
    pub history_sessions: usize,
    pub history_handles: usize,
    pub resolver_active_atoms: usize,
    pub pending_wire_atoms: usize,
    pub pending_resolver_wire_closes: usize,
    pub wire_handles: usize,
    pub wire_handle_demand_ref_handles: usize,
    pub wire_handle_demand_ref_keys: usize,
    pub wire_handle_demand_refs: usize,
    pub wire_handle_coverage_ref_handles: usize,
    pub wire_handle_coverage_ref_keys: usize,
    pub wire_handle_coverage_refs: usize,
    pub wire_owner_keys: usize,
    /// The sum of every live demand's owner count, not just how many demands
    /// are live. Without it the census cannot tell an owner count of two from
    /// one of four, and a rebuild that double-counted was invisible.
    pub wire_owner_refs: usize,
    pub wire_reverse_owner_keys: usize,
    pub wire_coverage_keys: usize,
    pub wire_coverage_edges: usize,
    pub wire_demand_keys: usize,
    pub wire_demand_edges: usize,
    pub wire_routing_evidence_keys: usize,
    pub wire_routing_evidence_facts: usize,
    pub wire_routing_evidence_refs: usize,
    pub active_physical_requests: usize,
    pub pending_execution_owner_keys: usize,
    pub pending_execution_owners: usize,
    pub request_attempts: usize,
    pub request_attempt_sub_keys: usize,
    pub request_attempt_sub_edges: usize,
    pub request_attempt_session_keys: usize,
    pub request_attempt_session_edges: usize,
    pub request_retry_jobs: usize,
    pub request_retry_sub_keys: usize,
    pub request_retry_session_keys: usize,
    pub request_retry_session_edges: usize,
    pub request_replacement_jobs: usize,
    pub request_replacement_session_keys: usize,
    pub request_replacement_session_edges: usize,
    pub active_execution_owners: usize,
    pub active_execution_owner_keys: usize,
    pub live_wire_owners: usize,
    pub pending_request_claim_transfer_jobs: usize,
    pub pending_request_claim_transfer_claims: usize,
    pub attribution_inflight_subs: usize,
    pub attribution_wire_keys: usize,
    pub attribution_shape_keys: usize,
    pub attribution_active_demands: usize,
    pub attribution_active_shape_keys: usize,
    pub attribution_active_shape_refs: usize,
    pub attribution_live_request_keys: usize,
    pub attribution_live_shape_keys: usize,
    pub attribution_live_shape_refs: usize,
    pub attribution_inflight_shape_keys: usize,
    pub attribution_inflight_shape_refs: usize,
    pub planned_read_sessions: usize,
    pub planned_read_relays: usize,
    pub plan_execution_metadata: usize,
    pub plan_execution_claims: usize,
    pub plan_execution_owner_demands: usize,
    pub active_nip77_live: usize,
    pub pending_neg_handoffs: usize,
    pub pending_neg_plan_keys: usize,
    pub pending_neg_plan_edges: usize,
    pub neg_sessions: usize,
    pub neg_session_plan_keys: usize,
    pub neg_session_plan_edges: usize,
    pub pending_backfills: usize,
    pub pending_backfill_plan_keys: usize,
    pub pending_backfill_plan_edges: usize,
    pub router_active_demands: usize,
    pub router_request_demand_keys: usize,
    pub router_request_demand_edges: usize,
    pub router_active_requests: usize,
    pub router_request_coverage_keys: usize,
    pub router_request_position_keys: usize,
    pub router_request_exact_filter_keys: usize,
    pub router_physical_request_claim_keys: usize,
    pub router_physical_claim_keys: usize,
    pub router_physical_claim_edges: usize,
    pub router_physical_request_contribution_keys: usize,
    pub router_physical_demand_keys: usize,
    pub router_physical_demand_edges: usize,
    pub router_request_owner_contribution_keys: usize,
    pub router_request_claim_owner_count_keys: usize,
    pub router_request_provenance_owner_count_keys: usize,
    pub router_request_demand_coverage_owner_count_keys: usize,
    pub router_coverage_assignment_keys: usize,
    pub router_coverage_assignment_edges: usize,
    pub router_refused_coverage_assignment_demands: usize,
    pub router_refused_coverage_assignment_authors: usize,
    pub router_active_outbox_authors: usize,
    pub router_refusal_demand_keys: usize,
    pub router_refusal_demand_edges: usize,
    pub router_refused_request_owner_keys: usize,
    pub router_refused_session_owner_keys: usize,
    pub router_diagnostic_author_session_keys: usize,
    pub router_diagnostic_author_edges: usize,
    pub router_uncovered_demand_keys: usize,
    pub router_uncovered_author_keys: usize,
    pub router_uncovered_author_refs: usize,
    pub router_plan_sessions: usize,
    pub router_plan_limited_demands: usize,
    pub router_plan_refused_sessions: usize,
    pub router_plan_subscription_shortfalls: usize,
    pub router_diagnostic_sessions: usize,
    pub router_diagnostic_uncovered_authors: usize,
    pub router_diagnostic_sessions_refused_by_cap: usize,
    pub router_diagnostic_sessions_refused_by_subscription_budget: usize,
    pub router_diagnostic_dropped_merge_rules: usize,
}

#[cfg(any(
    test,
    feature = "bench-instrumentation",
    feature = "test-instrumentation"
))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoreObservationOwnershipCensus {
    pub handles: usize,
    pub histories: usize,
    pub history_handles: usize,
    pub resolver_nodes: usize,
    pub demand_atoms: usize,
    pub planned_sessions: usize,
    pub pending_execution_owner_keys: usize,
    pub pending_execution_owners: usize,
    pub active_execution_owners: usize,
    pub live_wire_owners: usize,
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
    /// provider is still needed. Live `AuthorOutboxes` reads retain authors
    /// until a positive outbound route exists; zero-destination writes also
    /// retain settled zero-route contributors so a later positive replacement
    /// can unpark them. This is a need declaration, never a subscription;
    /// optional protocol assembly owns any exact query it opens.
    AuthorRouteNeedsChanged(BTreeSet<PublicKey>),
    /// -> `Pool::send` per (relay, current handle).
    Wire(AttemptedWireDelta),
    /// Reconnect: resend the current wire subs on the NEW generation of
    /// exactly this session.
    Replay(RelaySessionKey, AttemptedReplay),
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
    StartProbe(RequestAttemptId, RelayUrl, SubId, ConcreteFilter, String),
    /// Place a real `NEG-OPEN` after the live-first EOSE barrier for
    /// `filter` against a PROVEN-supported relay (ledger #8's compile-fence:
    /// the first field can only ever be a `ProbedRelay`), under its own
    /// NIP-77 `sub_id`, with the initial message built from the local store.
    NegOpen(RequestAttemptId, ProbedRelay, SubId, ConcreteFilter, String),
    /// Continue an open reconciliation: place this hex payload as the next
    /// outbound `NEG-MSG` for `sub_id` on `relay`.
    NegMsg(RequestAttemptId, RelayUrl, SubId, String),
    /// Release `sub_id` on `relay` (`NEG-CLOSE`) -- reconciliation finished,
    /// was abandoned (liveness deadline / `NEG-ERR`), or is being converted
    /// back to a plain REQ.
    NegClose(RelayUrl, SubId),
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
    /// Diagnostics state changed, but no projection has been materialized.
    /// Runtime coalesces this marker and builds the latest snapshot only at
    /// an observer delivery boundary; a reducer with no diagnostics observer
    /// therefore does no coverage or sibling-request work for this change.
    DiagnosticsChanged,
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
    /// see [`PublishError`] for the closed set of pre-custody failures.
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
pub struct RelayWorkerRequirements {
    pub all: BTreeSet<RelaySessionKey>,
    pub writes: BTreeSet<RelaySessionKey>,
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
#[derive(Clone)]
struct HandleAcquisition {
    scopes: Vec<ScopeAcquisition>,
}

/// One Demand boundary's freshness decision. Lifecycle ownership is
/// represented by variants, never a teardown bool: only `Live` contributes
/// that boundary's current atoms to the router; a coverage-satisfied scope
/// retains only the opening evidence that justified suppression.
#[derive(Clone)]
enum ScopeAcquisition {
    Live,
    CoverageSatisfied { evidence: AcquisitionEvidence },
    CacheOnly,
}

impl ScopeAcquisition {
    fn contributes_wire(&self) -> bool {
        matches!(self, Self::Live)
    }

    fn opening_evidence(&self) -> Option<&AcquisitionEvidence> {
        match self {
            Self::CoverageSatisfied { evidence, .. } => Some(evidence),
            Self::Live | Self::CacheOnly => None,
        }
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
    signature_state: RowSignature,
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
    /// Exact latest committed nonterminal row by relay. Scheduling is a
    /// reducer decision over this current state; the durable store remains
    /// the recovery authority, not a database that every heartbeat rereads.
    current_nonterminal: BTreeMap<RelayUrl, PublishQueueLane>,
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
            self.current_nonterminal.remove(&relay);
        } else {
            self.nonterminal.insert(relay.clone());
            self.current_nonterminal.insert(relay, lane.clone());
        }
        newly_persisted
    }

    fn mark_uncertain(&mut self, relay: RelayUrl) -> bool {
        self.current_nonterminal.remove(&relay);
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

#[derive(Clone)]
enum PendingWriteTarget {
    Event,
    ReplaceableOperation(Box<ReplaceableMaterializationTarget>),
}

impl PendingWriteTarget {
    fn accepts_ordinary_signer(&self) -> bool {
        match self {
            Self::Event => true,
            Self::ReplaceableOperation(target) => {
                // Reading the exact fence here is deliberate: semantic work
                // is parked for #1434, and cannot accidentally enter the
                // ordinary intent-id promotion path.
                let _exact_generation = target.expected_event_id;
                false
            }
        }
    }
}

struct PendingWrite {
    target: PendingWriteTarget,
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
    /// Signer identity selected and frozen at acceptance. Later current-
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
    started_at: Timestamp,
}

/// Current logical ownership attached to one immutable router-plan request.
///
/// NIP-77 role requests retain only `plan_sub_id`; every candidate, NEG, and
/// fallback generation snapshots this one record when it is sent. A later
/// byte-identical router metadata update mutates this record once and extends
/// the exact live child generations through the plan-to-role reverse indexes.
#[derive(Clone)]
struct PlanExecutionMetadata {
    filter: ConcreteFilter,
    coverage_claims: BTreeSet<CoverageKey>,
    owner_demands: BTreeSet<nmp_router::DemandKey>,
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

impl TemporaryReq {
    fn plan_sub_id(&self) -> &SubId {
        match self {
            Self::MissingIds { plan_sub_id, .. }
            | Self::Backlog { plan_sub_id }
            | Self::BacklogActivatesLive { plan_sub_id, .. } => plan_sub_id,
        }
    }
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

/// One post-settlement metadata addition that still owns an atomic durable
/// coverage transfer after the first store attempt failed.
#[derive(Debug)]
struct PendingRequestClaimTransfer {
    session: RelaySessionKey,
    sub_id: SubId,
    request_revision: u64,
    filter_hash: nmp_grammar::DescriptorHash,
    interval: CoverageInterval,
    claims: BTreeMap<CoverageKey, ContextualAtom>,
    due: Timestamp,
    failures: u32,
}

/// The synchronous reducer and durable-state owner (§2 position 1). No threads.
pub struct EngineCore {
    store: RedbStore,
    resolver: ResolverEngine,
    replaceable_materializers: HashMap<([u8; 16], [u8; 16]), ReplaceableMaterializerRegistration>,
    router: Router,
    routing_facts: RoutingFactStore,
    cap: usize,
    /// Per-BRANCH bookkeeping for every live observation branch, keyed by
    /// the resolver handle that owns it.
    handles: HashMap<HandleId, BranchState>,
    /// Which branch executes which filter path against which demand, and
    /// which of those are live. Three maps in two layers, private to
    /// `request_targets.rs`, so "forget every activation, keep every
    /// declaration" is one named operation rather than two hand-written
    /// clears a caller has to get both of.
    request_targets: RequestTargets,
    /// Which handles own which live-wire atoms, and which logical demands are
    /// therefore live. Ten maps, private to `wire_ownership.rs`, so owner
    /// counting has exactly one implementation instead of an incremental path
    /// and a rebuild that open-coded it a second time.
    wire: WireOwnership,
    /// Exact live-wire owner count per author contributed by
    /// `AuthorOutboxes` demand. This keeps neutral provider work incremental:
    /// unrelated handle teardown never scans the complete wire-demand set.
    author_outbox_wire_owner_counts: BTreeMap<PublicKey, usize>,
    /// Authors with live `AuthorOutboxes` demand and no positive outbound
    /// route. This is the read half of `AuthorRouteNeedsChanged`.
    author_outbox_route_needs: BTreeSet<PublicKey>,
    /// Whether an incremental wire-owner change altered that read half since
    /// the last provider-work edge was published.
    author_outbox_route_needs_changed: bool,
    /// Per-OBSERVATION delivered projection, keyed by the id every mailbox
    /// and cancellation uses.
    observations: HashMap<ObservationId, ObservationState>,
    next_observation_id: u64,
    /// Every open history window and the handle index that mirrors it
    /// (#1606 step 2). Private maps in `history_lifecycle.rs`, so I4 is
    /// maintained by one owner rather than at seven hand-written sites.
    history: HistorySessions,
    attribution: AttributionState,
    pending_request_evidence: HashMap<(RelaySessionKey, SubId), VecDeque<PendingRequestEvidence>>,
    /// Every local request-send attempt and the retries parked behind them
    /// (#1606 step 1). Its maps are private to `request_attempt.rs`, so the
    /// reverse-index invariants are enforced by the compiler rather than by
    /// every caller remembering them.
    attempts: RequestAttempts,
    pending_request_replacements: BTreeMap<SubId, nmp_router::RequestReplacement>,
    request_replacements_by_session: HashMap<RelaySessionKey, BTreeSet<SubId>>,
    active_request_evidence: HashMap<u64, ActiveRequestEvidence>,
    active_request_revisions_by_sub: HashMap<(RelaySessionKey, SubId), BTreeSet<u64>>,
    /// Exact REQs accepted by a live transport generation. Unlike request
    /// evidence, this survives EOSE because EOSE settles a request without
    /// closing its subscription.
    live_wire_requests: HashMap<(RelaySessionKey, SubId), LiveWireRequest>,
    /// The ordinary coordinate observations this reducer opened on behalf of
    /// semantic publish lanes, because nothing already covered the relay's
    /// current value for the coordinate (#1630/#1631).
    ///
    /// A delta generation must not be sent to a relay before that relay's
    /// current value for the coordinate is known, or it can overwrite a
    /// newer list only that relay holds. An observation stays open for as
    /// long as its receipt has work left on that relay, so a successor
    /// generation on the same lane reuses the same evidence instead of
    /// asking again — and so nothing leaks it, since no app subscription
    /// would ever withdraw it.
    ///
    /// Process-local by construction: nothing here is persisted and no
    /// verdict is cached. A restart simply repeats the ordinary check.
    semantic_publish_coverage: BTreeMap<(ReceiptId, RelayUrl), ObservationId>,
    /// The semantic publish lanes currently waiting on that answer.
    ///
    /// Separate from the observations because the two sets differ in both
    /// directions: a lane can wait on a request the app already owns
    /// (nothing opened), and a lane that has been answered keeps its
    /// observation while it is no longer waiting.
    semantic_publish_coverage_parked: BTreeSet<(ReceiptId, RelayUrl)>,
    /// Coverage observations retired while the current reducer turn is
    /// still draining their synchronous withdrawal effects.
    retired_coverage_observations: BTreeSet<ObservationId>,
    pending_request_claim_transfers:
        BTreeMap<(RelaySessionKey, SubId), PendingRequestClaimTransfer>,
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
    #[cfg(any(test, feature = "test-instrumentation"))]
    maintenance_turns: u64,
    active_pubkey: Option<PublicKey>,
    /// Publish queue (§3.4 / VISION §7 ledger #6/#9). `pending` is keyed by
    /// `ReceiptId` from `Publish` through to the last terminal per-relay
    /// status; `event_to_receipt` lets an inbound `OK` frame (keyed by
    /// `EventId` on the wire) find its receipt.
    pending: HashMap<ReceiptId, PendingWrite>,
    /// Last complete neutral author-route provider-work set published to the
    /// optional protocol assembly. The set is the union of unresolved write
    /// contributors and authors in live `AuthorOutboxes` reads without a
    /// positive outbound route. Keeping the prior value here makes provider
    /// synchronization an edge rather than a repeated side effect of every
    /// unrelated recompile.
    last_author_route_needs: BTreeSet<PublicKey>,
    /// The stalled-obligation census as of the last diagnostics snapshot
    /// this reducer PUSHED for a write-plane reason.
    ///
    /// A change detector for an observer, never a ledger: it holds no retry
    /// state, no history, and no fact that is not re-derivable from
    /// `pending` in one pass. Its only job is to keep an ordinary healthy
    /// publish from rebuilding an engine-global snapshot at every beat of a
    /// lifecycle in which nothing was ever stuck.
    last_stalled_write_census: Vec<(ReceiptId, StalledWriteStage)>,
    /// Materialized only when the stalled-write census changes. Diagnostics
    /// snapshots can be requested by unrelated read/query activity, so
    /// rebuilding and sorting every durable write on each request would put
    /// the entire publish queue on the read-plane hot path.
    cached_stalled_writes: Vec<StalledWrite>,
    cached_stalled_write_totals: StalledWriteTotals,
    /// Active durable obligations grouped by their final frozen event id.
    /// Used both to correlate relay OK frames after signing and, #903, to
    /// join an ordinary query row directly to every live receipt that owns
    /// those exact bytes. It includes signer-parked writes from acceptance
    /// onward, excludes terminal retained history, and is rebuilt from the
    /// store's open intents on every boot.
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
    /// intent's own terminal lane rows -- `RedbStore` only drops
    /// `PUBLISH_QUEUE_INTENTS`/the deadline indexes there, per that
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
    /// Exact shadow of router sessions used by incremental plan housekeeping.
    planned_read_sessions: BTreeSet<RelaySessionKey>,
    planned_read_session_counts_by_relay: BTreeMap<RelayUrl, usize>,
    /// Router plan request -> current logical metadata used by every NIP-77
    /// role generation derived from that immutable physical request.
    plan_execution_metadata: HashMap<SubId, PlanExecutionMetadata>,
    /// Every child subscription a router plan currently owns, plus which live
    /// REQ serves that plan's tail. Three (map, reverse index) pairs that had
    /// six verbatim-copied insert/take functions between them are one
    /// `PlanIndexed` in `nip77_sessions.rs`.
    nip77: Nip77Sessions,
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
    /// Degraded-store diagnostic retained from the first failed door in the
    /// current storage generation.
    ///
    /// A failure whose typed fault requires a fresh backend handle also arms
    /// `store_recovery_requested`. The runtime consumes that request and
    /// supervises bounded reconstruction; only a complete reopen plus
    /// store-derived reducer rebuild clears this diagnostic. Faults that do
    /// not require reopen remain observable but never trigger blind retry.
    ///
    /// Originally this was a permanent read-only latch (issue #122): set once the first time an
    /// ingest/read [`RedbStore`] door returns [`PersistenceError`] (disk
    /// full, I/O error). The reducer NEVER panics on such a failure — it
    /// records the error message here, skips the affected reactive step
    /// (leaving already-delivered state untouched rather than fabricating a
    /// phantom retraction), and surfaces it on the read-only diagnostics
    /// snapshot. A minimal, honest "the local cache went read-only" signal;
    /// #1362 closes that gap without making this string a control surface:
    /// recovery branches only on [`PersistenceFault`].
    store_degraded: Option<String>,
    /// Monotonic evidence that some store door failed. Recovery snapshots it
    /// before reducer reconstruction so a swallowed partial rebuild can never
    /// clear the diagnostic merely because its fault did not require reopen.
    store_failure_epoch: u64,
    /// `Option::take` request from the reducer to the one runtime supervisor.
    /// The fault is retained as typed evidence; its presence, never an
    /// adjacent boolean or diagnostic string, owns the recovery transition.
    store_recovery_requested: Option<PersistenceFault>,
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
    /// `nmp::EngineConfig::max_publish_attempts`. Counts
    /// observations, never wall-clock.
    max_publish_attempts: u64,
    /// Opt-in work counters for lifecycle attribution. Ordinary production
    /// builds pay no field or increment cost.
    #[cfg(any(test, feature = "bench-instrumentation"))]
    projection_store_queries: Cell<u64>,
    /// Ordinary REQs opened by [`Self::open_coordinate_observation`] because
    /// nothing already covered the coordinate (#1630). The reuse falsifier
    /// reads this: a second check for a covered coordinate must leave it at
    /// zero.
    #[cfg(any(test, feature = "bench-instrumentation"))]
    coordinate_reuse_new_reqs: Cell<u64>,
    #[cfg(any(test, feature = "bench-instrumentation"))]
    router_compiles: Cell<u64>,
    #[cfg(any(test, feature = "bench-instrumentation"))]
    history_store_queries: Cell<u64>,
    #[cfg(any(test, feature = "bench-instrumentation"))]
    withdrawal_handle_detaches: Cell<u64>,
    #[cfg(any(test, feature = "bench-instrumentation"))]
    resolver_delta_ops_consumed: Cell<u64>,
    #[cfg(any(test, feature = "bench-instrumentation"))]
    resolver_owner_keys_touched: Cell<u64>,
    #[cfg(any(test, feature = "bench-instrumentation"))]
    resolver_surviving_atoms_examined: Cell<u64>,
    #[cfg(any(test, feature = "bench-instrumentation"))]
    pending_atoms_rebuilt: Cell<u64>,
    #[cfg(any(test, feature = "bench-instrumentation"))]
    pending_cohort_atoms_reconciled: Cell<u64>,
    #[cfg(any(test, feature = "bench-instrumentation"))]
    attribution_atoms_rebuilt: Cell<u64>,
    #[cfg(any(test, feature = "bench-instrumentation"))]
    evidence_candidates_examined: Cell<u64>,
    #[cfg(any(test, feature = "bench-instrumentation"))]
    freshness_candidate_atoms: Cell<u64>,
    #[cfg(any(test, feature = "bench-instrumentation"))]
    freshness_incumbent_demand_edges_visited: Cell<u64>,
    #[cfg(any(test, feature = "bench-instrumentation"))]
    freshness_plan_request_entries_visited: Cell<u64>,
    #[cfg(any(test, feature = "bench-instrumentation"))]
    freshness_coalesce_pair_attempts: Cell<u64>,
    #[cfg(any(test, feature = "bench-instrumentation"))]
    request_target_demand_keys_touched: Cell<u64>,
    #[cfg(any(test, feature = "bench-instrumentation"))]
    request_target_candidates_examined: Cell<u64>,
    #[cfg(any(test, feature = "bench-instrumentation"))]
    request_claim_entries_examined: Cell<u64>,
    #[cfg(any(test, feature = "bench-instrumentation"))]
    request_owner_entries_examined: Cell<u64>,
    #[cfg(any(test, feature = "bench-instrumentation"))]
    request_claim_transfer_attempts: Cell<u64>,
    #[cfg(any(test, feature = "bench-instrumentation"))]
    request_claim_transfer_claims_attempted: Cell<u64>,
    #[cfg(any(test, feature = "bench-instrumentation"))]
    request_claim_transfer_commits: Cell<u64>,
    #[cfg(any(test, feature = "bench-instrumentation"))]
    request_claim_transfer_failures: Cell<u64>,
    #[cfg(any(test, feature = "bench-instrumentation"))]
    diagnostic_snapshots_built: Cell<u64>,
    #[cfg(any(test, feature = "bench-instrumentation"))]
    nip77_plan_children_touched: Cell<u64>,
    #[cfg(any(test, feature = "bench-instrumentation"))]
    routing_evidence_owner_keys_touched: Cell<u64>,
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
    lane: Option<(PublishQueueLaneKey, u64)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AttemptCorrelationExhausted;

impl EngineCore {
    pub fn install_replaceable_materializer(
        &mut self,
        registration: ReplaceableMaterializerRegistration,
    ) {
        self.replaceable_materializers
            .insert((registration.program, registration.format), registration);
    }

    pub fn install_replaceable_materializers(
        &mut self,
        capabilities: Vec<nmp_grammar::ReplaceableMaterializerSpec>,
    ) {
        for spec in capabilities {
            self.install_replaceable_materializer(spec.into_registration());
        }
    }

    pub fn new(store: RedbStore, cap: usize) -> Self {
        Self::new_with_routing_facts(store, RoutingFactStore::default(), cap)
    }

    /// Construct a headless reducer over a static fact snapshot.
    ///
    /// This exists for deterministic falsifiers. Production assembly owns
    /// the private mutable fact store and uses [`Self::new`].
    #[cfg(feature = "unstable-mechanism")]
    #[doc(hidden)]
    pub fn new_with_fixture_routing_facts(
        store: RedbStore,
        facts: nmp_router_testkit::FixtureRoutingFacts,
        cap: usize,
    ) -> Self {
        Self::new_with_routing_facts(store, RoutingFactStore::from_fixture(facts), cap)
    }

    pub fn new_with_routing_facts(
        store: RedbStore,
        routing_facts: RoutingFactStore,
        cap: usize,
    ) -> Self {
        Self {
            store,
            resolver: ResolverEngine::new(),
            replaceable_materializers: HashMap::new(),
            router: Router::new(RuleRegistry::default_widen_only()),
            routing_facts,
            cap,
            handles: HashMap::new(),
            request_targets: RequestTargets::default(),
            wire: WireOwnership::default(),
            author_outbox_wire_owner_counts: BTreeMap::new(),
            author_outbox_route_needs: BTreeSet::new(),
            author_outbox_route_needs_changed: false,
            observations: HashMap::new(),
            next_observation_id: 0,
            history: HistorySessions::new(),
            attribution: AttributionState::new(),
            pending_request_evidence: HashMap::new(),
            attempts: RequestAttempts::new(),
            pending_request_replacements: BTreeMap::new(),
            request_replacements_by_session: HashMap::new(),
            active_request_evidence: HashMap::new(),
            active_request_revisions_by_sub: HashMap::new(),
            live_wire_requests: HashMap::new(),
            semantic_publish_coverage: BTreeMap::new(),
            semantic_publish_coverage_parked: BTreeSet::new(),
            retired_coverage_observations: BTreeSet::new(),
            pending_request_claim_transfers: BTreeMap::new(),
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
            #[cfg(any(test, feature = "test-instrumentation"))]
            maintenance_turns: 0,
            active_pubkey: None,
            pending: HashMap::new(),
            last_author_route_needs: BTreeSet::new(),
            last_stalled_write_census: Vec::new(),
            cached_stalled_writes: Vec::new(),
            cached_stalled_write_totals: StalledWriteTotals {
                detail_limit: u64::try_from(STALLED_WRITE_DETAIL_LIMIT).unwrap_or(u64::MAX),
                ..StalledWriteTotals::default()
            },
            event_to_receipts: HashMap::new(),
            intent_receipts: HashMap::new(),
            receipts_by_lane_relay: HashMap::new(),
            lane_relay_index_degraded: false,
            lane_projection_unprovable: false,
            lane_bootstrap_retries: BTreeMap::new(),
            prober: Prober::new(),
            nip11_information: HashMap::new(),
            planned_read_sessions: BTreeSet::new(),
            planned_read_session_counts_by_relay: BTreeMap::new(),
            plan_execution_metadata: HashMap::new(),
            nip77: Nip77Sessions::default(),
            events_by_session_kind: HashMap::new(),
            next_attempt_correlation: Some(0),
            attempt_correlations: HashMap::new(),
            store_degraded: None,
            store_failure_epoch: 0,
            store_recovery_requested: None,
            relay_open_failures: BTreeMap::new(),
            transport_degraded: None,
            retry_scheduler_blocked: false,
            max_publish_attempts: crate::publish_queue::DEFAULT_MAX_PUBLISH_ATTEMPTS,
            #[cfg(any(test, feature = "bench-instrumentation"))]
            projection_store_queries: Cell::new(0),
            #[cfg(any(test, feature = "bench-instrumentation"))]
            coordinate_reuse_new_reqs: Cell::new(0),
            #[cfg(any(test, feature = "bench-instrumentation"))]
            router_compiles: Cell::new(0),
            #[cfg(any(test, feature = "bench-instrumentation"))]
            history_store_queries: Cell::new(0),
            #[cfg(any(test, feature = "bench-instrumentation"))]
            withdrawal_handle_detaches: Cell::new(0),
            #[cfg(any(test, feature = "bench-instrumentation"))]
            resolver_delta_ops_consumed: Cell::new(0),
            #[cfg(any(test, feature = "bench-instrumentation"))]
            resolver_owner_keys_touched: Cell::new(0),
            #[cfg(any(test, feature = "bench-instrumentation"))]
            resolver_surviving_atoms_examined: Cell::new(0),
            #[cfg(any(test, feature = "bench-instrumentation"))]
            pending_atoms_rebuilt: Cell::new(0),
            #[cfg(any(test, feature = "bench-instrumentation"))]
            pending_cohort_atoms_reconciled: Cell::new(0),
            #[cfg(any(test, feature = "bench-instrumentation"))]
            attribution_atoms_rebuilt: Cell::new(0),
            #[cfg(any(test, feature = "bench-instrumentation"))]
            evidence_candidates_examined: Cell::new(0),
            #[cfg(any(test, feature = "bench-instrumentation"))]
            freshness_candidate_atoms: Cell::new(0),
            #[cfg(any(test, feature = "bench-instrumentation"))]
            freshness_incumbent_demand_edges_visited: Cell::new(0),
            #[cfg(any(test, feature = "bench-instrumentation"))]
            freshness_plan_request_entries_visited: Cell::new(0),
            #[cfg(any(test, feature = "bench-instrumentation"))]
            freshness_coalesce_pair_attempts: Cell::new(0),
            #[cfg(any(test, feature = "bench-instrumentation"))]
            request_target_demand_keys_touched: Cell::new(0),
            #[cfg(any(test, feature = "bench-instrumentation"))]
            request_target_candidates_examined: Cell::new(0),
            #[cfg(any(test, feature = "bench-instrumentation"))]
            request_claim_entries_examined: Cell::new(0),
            #[cfg(any(test, feature = "bench-instrumentation"))]
            request_owner_entries_examined: Cell::new(0),
            #[cfg(any(test, feature = "bench-instrumentation"))]
            request_claim_transfer_attempts: Cell::new(0),
            #[cfg(any(test, feature = "bench-instrumentation"))]
            request_claim_transfer_claims_attempted: Cell::new(0),
            #[cfg(any(test, feature = "bench-instrumentation"))]
            request_claim_transfer_commits: Cell::new(0),
            #[cfg(any(test, feature = "bench-instrumentation"))]
            request_claim_transfer_failures: Cell::new(0),
            #[cfg(any(test, feature = "bench-instrumentation"))]
            diagnostic_snapshots_built: Cell::new(0),
            #[cfg(any(test, feature = "bench-instrumentation"))]
            nip77_plan_children_touched: Cell::new(0),
            #[cfg(any(test, feature = "bench-instrumentation"))]
            routing_evidence_owner_keys_touched: Cell::new(0),
            #[cfg(test)]
            history_rows_examined: Cell::new(0),
            #[cfg(test)]
            history_affected_row_queries: Cell::new(0),
        }
    }

    /// The sole neutral author-route mutation door. Replacement and the
    /// resulting Auto-write wake happen in one reducer turn.
    #[allow(dead_code)]
    pub fn replace_author_routes(
        &mut self,
        author: PublicKey,
        replacement: AuthorRouteReplacement,
        effects: &mut Vec<Effect>,
    ) {
        let before = self.routing_facts.author_routes(&author);
        self.routing_facts.writer().replace(author, replacement);
        if self.routing_facts.author_routes(&author) != before {
            self.recompile(effects);
            self.rewrite_open_routes(effects);
        }
    }

    /// Set the per-relay attempt ceiling (#1031). Zero is refused into the
    /// finite default: a ceiling of zero would give up before ever trying,
    /// which is a verdict without a single observation behind it.
    #[must_use]
    pub fn with_max_publish_attempts(mut self, max_publish_attempts: u64) -> Self {
        self.max_publish_attempts = if max_publish_attempts == 0 {
            crate::publish_queue::DEFAULT_MAX_PUBLISH_ATTEMPTS
        } else {
            max_publish_attempts
        };
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
    /// This projection is computed from in-memory state alone: durable lane
    /// reads happen only at bootstrap/recovery and mutation boundaries, never
    /// while reconciling ordinary worker ownership. ("Pure" would be the wrong
    /// word for anything in `EngineCore`, which owns the store and commits
    /// through it -- see `docs/internals/architecture-boundaries.md`.)
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

    pub fn relay_worker_requirements(&self) -> Option<RelayWorkerRequirements> {
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

    /// Assert every extracted owner's mirrors are exactly right, by identity
    /// rather than by count.
    ///
    /// The census next to this counts things. That is the correct instrument
    /// for leaks and for boundedness, and the wrong one for structure: a
    /// handle indexed under the wrong atom, a child under the wrong plan, or a
    /// live target under the wrong demand all preserve every number it
    /// reports. Tests that care about structure call this; tests that care
    /// about totals call the census; nothing should use one for the other.
    #[cfg(any(test, feature = "bench-instrumentation"))]
    pub fn assert_owner_consistency(&self, at: &str) {
        self.wire.assert_consistent(at);
        self.request_targets.assert_consistent(at);
        self.nip77.assert_consistent(at);
    }

    #[cfg(any(test, feature = "bench-instrumentation"))]
    #[doc(hidden)]
    pub fn bench_ownership_census(&self) -> CoreOwnershipCensus {
        let (
            attribution_inflight_subs,
            attribution_wire_keys,
            attribution_shape_keys,
            attribution_active_demands,
            attribution_active_shape_keys,
            attribution_active_shape_refs,
            attribution_live_request_keys,
            attribution_live_shape_keys,
            attribution_live_shape_refs,
            attribution_inflight_shape_keys,
            attribution_inflight_shape_refs,
        ) = self.attribution.ownership_census();
        let router = self.router.ownership_census();
        let attempts = self.attempts.counts();
        let history = self.history.counts();
        let wire = self.wire.counts();
        let targets = self.request_targets.counts();
        let nip77 = self.nip77.counts();
        CoreOwnershipCensus {
            observations: self.observations.len(),
            branch_handles: self.handles.len(),
            retained_freshness_source_edges: self
                .handles
                .values()
                .flat_map(|state| &state.acquisition.scopes)
                .filter_map(ScopeAcquisition::opening_evidence)
                .map(|evidence| evidence.sources.len())
                .sum::<usize>()
                + self.history.freshness_source_edges(),
            request_target_handles: targets.handles,
            request_target_demand_keys: targets.demand_keys,
            request_target_edges: targets.edges,
            request_target_refs: targets.refs,
            active_request_target_handles: targets.active_handles,
            active_request_target_demand_keys: targets.active_demand_keys,
            active_request_target_edges: targets.active_edges,
            active_request_target_refs: targets.active_refs,
            history_sessions: history.sessions,
            history_handles: history.handles,
            resolver_active_atoms: self.resolver.active_demand().len(),
            pending_wire_atoms: wire.pending_atoms,
            pending_resolver_wire_closes: wire.pending_resolver_closes,
            wire_handles: wire.handles,
            wire_handle_demand_ref_handles: wire.demand_ref_handles,
            wire_handle_demand_ref_keys: wire.demand_ref_keys,
            wire_handle_demand_refs: wire.demand_refs,
            wire_handle_coverage_ref_handles: wire.coverage_ref_handles,
            wire_handle_coverage_ref_keys: wire.coverage_ref_keys,
            wire_handle_coverage_refs: wire.coverage_refs,
            wire_owner_keys: wire.owner_keys,
            wire_owner_refs: wire.owner_refs,
            wire_reverse_owner_keys: wire.reverse_owner_keys,
            wire_coverage_keys: wire.coverage_keys,
            wire_coverage_edges: wire.coverage_edges,
            wire_demand_keys: wire.demand_keys,
            wire_demand_edges: wire.demand_edges,
            wire_routing_evidence_keys: wire.routing_evidence_keys,
            wire_routing_evidence_facts: wire.routing_evidence_facts,
            wire_routing_evidence_refs: wire.routing_evidence_refs,
            active_physical_requests: router.plan_requests,
            pending_execution_owner_keys: self.pending_request_evidence.len(),
            pending_execution_owners: self
                .pending_request_evidence
                .values()
                .map(VecDeque::len)
                .sum(),
            request_attempts: attempts.attempts,
            request_attempt_sub_keys: attempts.sub_keys,
            request_attempt_sub_edges: attempts.sub_edges,
            request_attempt_session_keys: attempts.session_keys,
            request_attempt_session_edges: attempts.session_edges,
            request_retry_jobs: attempts.retry_jobs,
            request_retry_sub_keys: attempts.retry_sub_keys,
            request_retry_session_keys: attempts.retry_session_keys,
            request_retry_session_edges: attempts.retry_session_edges,
            request_replacement_jobs: self.pending_request_replacements.len(),
            request_replacement_session_keys: self.request_replacements_by_session.len(),
            request_replacement_session_edges: self
                .request_replacements_by_session
                .values()
                .map(BTreeSet::len)
                .sum(),
            active_execution_owners: self.active_request_evidence.len(),
            active_execution_owner_keys: self.active_request_revisions_by_sub.len(),
            live_wire_owners: self.live_wire_requests.len(),
            pending_request_claim_transfer_jobs: self.pending_request_claim_transfers.len(),
            pending_request_claim_transfer_claims: self
                .pending_request_claim_transfers
                .values()
                .map(|pending| pending.claims.len())
                .sum(),
            attribution_inflight_subs,
            attribution_wire_keys,
            attribution_shape_keys,
            attribution_active_demands,
            attribution_active_shape_keys,
            attribution_active_shape_refs,
            attribution_live_request_keys,
            attribution_live_shape_keys,
            attribution_live_shape_refs,
            attribution_inflight_shape_keys,
            attribution_inflight_shape_refs,
            planned_read_sessions: self.planned_read_sessions.len(),
            planned_read_relays: self.planned_read_session_counts_by_relay.len(),
            plan_execution_metadata: self.plan_execution_metadata.len(),
            plan_execution_claims: self
                .plan_execution_metadata
                .values()
                .map(|metadata| metadata.coverage_claims.len())
                .sum(),
            plan_execution_owner_demands: self
                .plan_execution_metadata
                .values()
                .map(|metadata| metadata.owner_demands.len())
                .sum(),
            active_nip77_live: nip77.live,
            pending_neg_handoffs: nip77.handoffs,
            pending_neg_plan_keys: nip77.handoff_plan_keys,
            pending_neg_plan_edges: nip77.handoff_plan_edges,
            neg_sessions: nip77.sessions,
            neg_session_plan_keys: nip77.session_plan_keys,
            neg_session_plan_edges: nip77.session_plan_edges,
            pending_backfills: nip77.backfills,
            pending_backfill_plan_keys: nip77.backfill_plan_keys,
            pending_backfill_plan_edges: nip77.backfill_plan_edges,
            router_active_demands: router.active_demands,
            router_request_demand_keys: router.requests_by_demand_keys,
            router_request_demand_edges: router.requests_by_demand_edges,
            router_active_requests: router.active_by_request,
            router_request_coverage_keys: router.request_coverage_keys,
            router_request_position_keys: router.request_position_keys,
            router_request_exact_filter_keys: router.request_exact_filter_keys,
            router_physical_request_claim_keys: router.physical_request_claim_keys,
            router_physical_claim_keys: router.physical_claim_keys,
            router_physical_claim_edges: router.physical_claim_edges,
            router_physical_request_contribution_keys: router.physical_request_contribution_keys,
            router_physical_demand_keys: router.physical_demand_keys,
            router_physical_demand_edges: router.physical_demand_edges,
            router_request_owner_contribution_keys: router.request_owner_contribution_keys,
            router_request_claim_owner_count_keys: router.request_claim_owner_count_keys,
            router_request_provenance_owner_count_keys: router.request_provenance_owner_count_keys,
            router_request_demand_coverage_owner_count_keys: router
                .request_demand_coverage_owner_count_keys,
            router_coverage_assignment_keys: router.coverage_assignment_keys,
            router_coverage_assignment_edges: router.coverage_assignment_edges,
            router_refused_coverage_assignment_demands: router.refused_coverage_assignment_demands,
            router_refused_coverage_assignment_authors: router.refused_coverage_assignment_authors,
            router_active_outbox_authors: router.active_outbox_authors,
            router_refusal_demand_keys: router.refusal_demand_keys,
            router_refusal_demand_edges: router.refusal_demand_edges,
            router_refused_request_owner_keys: router.refused_request_owner_keys,
            router_refused_session_owner_keys: router.refused_session_owner_keys,
            router_diagnostic_author_session_keys: router.diagnostic_author_session_keys,
            router_diagnostic_author_edges: router.diagnostic_author_edges,
            router_uncovered_demand_keys: router.uncovered_demand_keys,
            router_uncovered_author_keys: router.uncovered_author_keys,
            router_uncovered_author_refs: router.uncovered_author_refs,
            router_plan_sessions: router.plan_sessions,
            router_plan_limited_demands: router.plan_limited_demands,
            router_plan_refused_sessions: router.plan_refused_sessions,
            router_plan_subscription_shortfalls: router.plan_subscription_shortfalls,
            router_diagnostic_sessions: router.diagnostic_sessions,
            router_diagnostic_uncovered_authors: router.diagnostic_uncovered_authors,
            router_diagnostic_sessions_refused_by_cap: router.diagnostic_sessions_refused_by_cap,
            router_diagnostic_sessions_refused_by_subscription_budget: router
                .diagnostic_sessions_refused_by_subscription_budget,
            router_diagnostic_dropped_merge_rules: router.diagnostic_dropped_merge_rules,
        }
    }

    #[cfg(any(
        test,
        feature = "bench-instrumentation",
        feature = "test-instrumentation"
    ))]
    pub fn observation_ownership_census(&self) -> CoreObservationOwnershipCensus {
        let history = self.history.counts();
        CoreObservationOwnershipCensus {
            handles: self.handles.len(),
            histories: history.sessions,
            history_handles: history.handles,
            resolver_nodes: self.resolver.graph_snapshot().nodes.len(),
            demand_atoms: self.active_demand().len(),
            planned_sessions: self.router.plan().reqs.len(),
            pending_execution_owner_keys: self.pending_request_evidence.len(),
            pending_execution_owners: self
                .pending_request_evidence
                .values()
                .map(VecDeque::len)
                .sum(),
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
        self.store
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
        #[cfg(any(test, feature = "bench-instrumentation"))]
        self.diagnostic_snapshots_built
            .set(self.diagnostic_snapshots_built.get().saturating_add(1));
        let mut snapshot = diagnostics::build(
            self.router.diagnostics(),
            self.router.plan(),
            &self.events_by_session_kind,
            |relay, key| self.store.get_coverage(key, relay),
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
                },
            );
        }
        for (session, state) in &self.auth_sessions {
            let (phase, auth_event_id) = match &state.phase {
                AuthSessionPhase::AwaitingPolicy { .. } => {
                    (AuthDiagnosticsPhase::AwaitingPolicy, None)
                }
                AuthSessionPhase::AwaitingSignature { .. } => {
                    (AuthDiagnosticsPhase::AwaitingSignature, None)
                }
                AuthSessionPhase::AwaitingSend { event_id, .. } => {
                    (AuthDiagnosticsPhase::AwaitingSend, Some(*event_id))
                }
                AuthSessionPhase::AwaitingOk { event_id } => {
                    (AuthDiagnosticsPhase::AwaitingRelayAck, Some(*event_id))
                }
                AuthSessionPhase::Ready { event_id } => {
                    (AuthDiagnosticsPhase::Ready, Some(*event_id))
                }
                AuthSessionPhase::Denied => (AuthDiagnosticsPhase::Denied, None),
                AuthSessionPhase::Error => (AuthDiagnosticsPhase::Error, None),
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
                },
            );
        }
        snapshot.auth_sessions = auth_sessions.into_values().collect();
        snapshot.stalled_writes = self.cached_stalled_writes.clone();
        snapshot.stalled_write_totals = self.cached_stalled_write_totals;
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
                relay.nip11_last_error = information.last_error.clone();
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
            relay.nip77_handoff = if self.nip77.backfills.iter().any(|(sub_id, request)| {
                sub_id.0 == relay.relay
                    && matches!(
                        request,
                        TemporaryReq::Backlog { .. } | TemporaryReq::BacklogActivatesLive { .. }
                    )
            }) {
                "fallback_backlog"
            } else if self.nip77.backfills.iter().any(|(sub_id, request)| {
                sub_id.0 == relay.relay && matches!(request, TemporaryReq::MissingIds { .. })
            }) {
                "backfilling"
            } else if self
                .nip77
                .sessions
                .iter()
                .any(|(_, session)| session.relay == relay.relay)
            {
                "reconciling"
            } else if self
                .nip77
                .handoffs
                .iter()
                .any(|(sub_id, _)| sub_id.0 == relay.relay)
            {
                "awaiting_live_eose"
            } else if self.nip77.has_live_on_relay(&relay.relay)
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

    /// A pure clock update plus the owned deadline sweeps: failed
    /// post-settlement request-claim transfers, NIP-40 expiry
    /// (retraction-and-negative-deltas.md §3.2 — drains `store.expire_due`
    /// and retracts every row past its deadline) and the negentropy
    /// liveness-deadline sweep (plan §6 E, harvest `nmp-nip77`'s "30s
    /// liveness-deadline REQ fallback"): any reconciliation session open
    /// longer than [`NEG_LIVENESS_DEADLINE_SECS`] against `now` is
    /// abandoned in favor of a plain REQ for the same (unfloored/unlimited)
    /// filter. Claim-transfer retry records retain the exact request revision,
    /// committed interval, and atom payload through capped backoff. The same
    /// tick also consumes every due durable-lane retry/ACK deadline through
    /// the one delivery scheduler.
    ///
    /// `runtime::engine_loop` (§3.3, #39) is what actually drives this on
    /// its own now: it arms `cmd_rx.recv_timeout` off [`Self::next_deadline`]
    /// and dispatches `EngineMsg::Tick(wall_now())` exactly when that
    /// timeout elapses (D8: the existing blocking recv grows a timeout,
    /// never a poll-loop timer thread). Every sweep stays real and unit-
    /// tested here against a synthetic clock regardless of who calls this
    /// -- the runtime driver is a caller, not part of the mechanism.
    pub fn tick(&mut self, now: Timestamp) -> Vec<Effect> {
        #[cfg(any(test, feature = "test-instrumentation"))]
        {
            self.maintenance_turns = self.maintenance_turns.saturating_add(1);
        }
        self.clock = now;
        let mut effects = Vec::new();
        self.retry_scheduler_blocked = false;
        self.retry_due_request_claim_transfers(now, &mut effects);
        self.retry_due_request_attempts(now, &mut effects);
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
        match self.store.expire_due(now) {
            Ok(expired) if !expired.is_empty() => {
                let removed: Vec<_> = expired.into_iter().map(|se| se.event).collect();
                match self.resolver.retract(&self.store, removed) {
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
        let stale_handoffs = self
            .nip77
            .handoffs
            .take_where(|_, handoff| now >= handoff.started_at + NEG_LIVENESS_DEADLINE_SECS);
        for (_, handoff) in stale_handoffs {
            self.handoff_fallback_to_req(handoff, &mut effects);
        }

        let stale_neg = self
            .nip77
            .sessions
            .take_where(|_, session| now >= session.started_at + NEG_LIVENESS_DEADLINE_SECS);
        for (sub_id, session) in stale_neg {
            self.neg_session_fallback_to_req(sub_id, session, &mut effects);
        }

        effects
    }

    #[cfg(any(test, feature = "test-instrumentation"))]
    pub fn maintenance_turn_count(&self) -> u64 {
        self.maintenance_turns
    }

    /// Advance reducer wall-clock truth without executing any deadline work.
    /// Runtime does this once at command boundaries; due expiry, retry, and
    /// liveness work remain exclusively owned by [`Self::tick`] and
    /// [`Self::next_deadline`].
    pub fn advance_clock(&mut self, now: Timestamp) {
        self.clock = now;
    }

    /// The reducer's own current wall truth. Effect dispatch opens the NIP-65
    /// route-source observation with this rather than re-reading a clock the
    /// reducer has not seen yet -- the same value [`Self::on_subscribe`] uses
    /// for an app subscription.
    pub fn clock(&self) -> Timestamp {
        self.clock
    }

    /// The earliest wall-clock instant at which [`Self::tick`] must run for
    /// something to actually happen (retraction-and-negative-deltas.md
    /// §3.2): the min over every deadline source this reducer currently
    /// tracks -- NIP-40 expiry (`store.next_expiration()`, index-backed),
    /// open negentropy sessions' liveness deadlines (`started_at +
    /// NEG_LIVENESS_DEADLINE_SECS`), and request-claim transfer backoff.
    /// `None` means no timer needs to fire at
    /// all right now: `runtime::engine_loop`'s `recv_timeout` driver (§3.3)
    /// sleeps forever on the plain `recv()` in that case, exactly matching
    /// the doc's "a light embedder with no deadlines pays nothing".
    /// Extensible to future timers (drop-grace debounce) by folding
    /// another `.min()` term in here -- the runtime driver itself never
    /// needs to change to pick up a new deadline source.
    ///
    /// The durable terms are fallible, and this door hands their failures
    /// straight to its caller rather than folding
    /// them into `None` (#763). The distinction is the whole point: `Ok(None)`
    /// tells the driver to park on a plain `recv()` forever, which is correct
    /// only when there is genuinely nothing to wake up for. A read that could
    /// not answer is not that, and the delivery term reaching here as
    /// `.ok().flatten()` is how a durable, due obligation could stop being
    /// scheduled with nothing recording why. `runtime::engine_loop` degrades
    /// the store on `Err`, which is the #122 fact an app already reads.
    pub fn next_deadline(&self) -> Result<Option<Timestamp>, PersistenceError> {
        let expiry = self.store.next_expiration()?;
        let neg_liveness = self
            .nip77
            .sessions
            .iter()
            .map(|(_, session)| session.started_at + NEG_LIVENESS_DEADLINE_SECS)
            .chain(
                self.nip77
                    .handoffs
                    .iter()
                    .map(|(_, handoff)| handoff.started_at + NEG_LIVENESS_DEADLINE_SECS),
            )
            .min();
        // A persistence failure already latched by the write plane suppresses
        // this term until real work arrives (`handle` clears the flag), which
        // is a recorded decision rather than an erased read. The read itself
        // still propagates.
        let delivery = match (!self.retry_scheduler_blocked)
            .then(|| self.store.next_publish_queue_deadline())
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
        let request_claim_transfer = self
            .pending_request_claim_transfers
            .values()
            .map(|pending| pending.due)
            .min();
        let request_retry = self.attempts.next_retry_due();
        Ok([
            expiry,
            neg_liveness,
            delivery,
            bootstrap,
            request_claim_transfer,
            request_retry,
        ]
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
        let effects = match msg {
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
                    let mut effects = vec![Effect::EmitDiagnostics(self.diagnostics_snapshot())];
                    self.refresh_all_observations(&mut effects);
                    self.refresh_all_histories(&mut effects);
                    effects
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
        // A semantic publish lane parked on the ordinary query owner's
        // answer for its coordinate is waiting on a READ terminal, and
        // every one of the write plane's own scheduling beats is driven by
        // a write-plane fact. Without this, the relay that answers the
        // question would never wake the lane that asked it. The guard is
        // the question itself: with nothing parked there is nothing to
        // re-ask, so an ordinary turn pays one map emptiness check.
        let mut effects = effects;
        if self.has_parked_coordinate_coverage() {
            let ready = self.schedule_ready(self.clock);
            effects.extend(ready);
        }
        // The coordinate-coverage observations the publish gate opens use
        // the ordinary resolver/router path, but no app mailbox owns them:
        // their rows and evidence belong to the write plane. Drop only those
        // private delivery effects and leave every ordinary observation
        // untouched.
        let mut effects = self.consume_coverage_observation_effects(effects);
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
        if self.refresh_stalled_write_cache_for_effects(&effects) {
            effects.push(Effect::DiagnosticsChanged);
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
        // A frame the transport rejected or could not decode before this
        // reducer could see it is a returned EVENT frame nothing counted
        // (#1630, #1668). Health is the only place either is ever reported,
        // and neither names a subscription, so every request still streaming
        // on this session loses its exact count.
        //
        // The undecodable tally is its own field rather than part of the
        // misbehavior count: a malformed line is not a forgery, and a caller
        // deciding whether to stop trusting a relay must not see the two as
        // the same signal.
        if health.invalid_signature_count > 0
            || health.undecodable_frame_count > 0
            || health.last_error.is_some()
        {
            self.erase_returned_frame_counts(&session);
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

    /// The current account, for the runtime's own reads (#1657).
    ///
    /// This is the one stored copy. The reducer must hold it because it
    /// resolves `Identity::Active` and re-roots reactive bindings from pure
    /// `&mut self` code that cannot reach the runtime's account registry, and
    /// because `EngineCore` is exercised headlessly with no runtime in
    /// existence at all. `RuntimeSessionState` therefore keeps no second
    /// copy: it owns the account set and asks here for the selection.
    pub fn active_pubkey(&self) -> Option<PublicKey> {
        self.active_pubkey
    }

    fn on_set_active_pubkey(&mut self, pk: Option<PublicKey>) -> Vec<Effect> {
        self.active_pubkey = pk;
        let mut effects = Vec::new();
        // Re-rooting reactive nodes can re-query the store (a `Derived`
        // binding over a reactive field). Degrade to read-only on a
        // persistence failure (issue #122) rather than panic.
        if let Err(e) = self.resolver.set_active_pubkey(&self.store, pk) {
            self.degrade_store(e, &mut effects);
            return effects;
        }
        let ids: Vec<_> = self.handles.keys().copied().collect();
        for id in ids {
            self.reconcile_observation_resolution(
                id,
                ResolutionCause::CurrentAccountChanged,
                &mut effects,
            );
        }
        self.recompile(&mut effects);
        if let Some(pk) = pk {
            // The runtime moves its current signing provider pointer before delivering
            // this message. Re-arm matching accepted work here as well as
            // on SignerAttached so both ordering cases (activate→attach and
            // attach→activate) converge without polling.
            effects.extend(self.on_signer_attached(pk));
        }
        effects
    }
}

#[cfg(any(test, feature = "test-instrumentation"))]
impl EngineCore {
    /// Execute the runtime's existing requested Redb reconstruction sequence
    /// without exposing its two internal lifecycle doors independently.
    #[doc(hidden)]
    pub fn recover_requested_redb_store_for_test(
        &mut self,
    ) -> Result<Option<(PersistenceFault, Vec<Effect>)>, PersistenceError> {
        let Some(fault) = self.take_store_recovery_request() else {
            return Ok(None);
        };
        self.recover_store_after_failure()
            .map(|effects| Some((fault, effects)))
    }

    /// Test-only access to the concrete store-door count used by the relay
    /// worker scheduling falsifiers.
    #[doc(hidden)]
    pub fn reset_publish_queue_lane_recovery_reads(&self) {
        self.store.reset_publish_queue_lane_recovery_reads();
    }

    #[doc(hidden)]
    pub fn publish_queue_lane_recovery_reads(&self) -> u64 {
        self.store.publish_queue_lane_recovery_reads()
    }
}

#[cfg(feature = "bench-instrumentation")]
impl EngineCore {
    /// Reset reducer lifecycle counters independently from Redb's row-work
    /// counters so a benchmark can attribute admission and projection work.
    #[doc(hidden)]
    pub fn bench_reset_lifecycle_work(&self) {
        self.projection_store_queries.set(0);
        self.router_compiles.set(0);
        self.history_store_queries.set(0);
        self.pending_atoms_rebuilt.set(0);
        self.pending_cohort_atoms_reconciled.set(0);
        self.attribution_atoms_rebuilt.set(0);
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

    /// `(whole pending-owner rebuild visits, submitted cohort atoms
    /// reconciled, whole attribution-demand rebuild visits)` since reset.
    #[doc(hidden)]
    pub fn bench_admission_local_work(&self) -> (u64, u64, u64) {
        (
            self.pending_atoms_rebuilt.get(),
            self.pending_cohort_atoms_reconciled.get(),
            self.attribution_atoms_rebuilt.get(),
        )
    }

    #[doc(hidden)]
    pub fn bench_reset_admission_work(&mut self) {
        self.pending_atoms_rebuilt.set(0);
        self.pending_cohort_atoms_reconciled.set(0);
        self.attribution_atoms_rebuilt.set(0);
        self.evidence_candidates_examined.set(0);
        self.request_target_demand_keys_touched.set(0);
        self.request_target_candidates_examined.set(0);
        self.request_claim_entries_examined.set(0);
        self.request_owner_entries_examined.set(0);
        self.request_claim_transfer_attempts.set(0);
        self.request_claim_transfer_claims_attempted.set(0);
        self.request_claim_transfer_commits.set(0);
        self.request_claim_transfer_failures.set(0);
        self.diagnostic_snapshots_built.set(0);
        self.router.reset_admission_work();
    }

    #[doc(hidden)]
    pub fn bench_admission_work(&self) -> CoreAdmissionWork {
        let router = self.router.admission_work();
        CoreAdmissionWork {
            pending_atoms_rebuilt: self.pending_atoms_rebuilt.get(),
            pending_cohort_atoms_reconciled: self.pending_cohort_atoms_reconciled.get(),
            attribution_atoms_rebuilt: self.attribution_atoms_rebuilt.get(),
            evidence_candidates_examined: self.evidence_candidates_examined.get(),
            request_target_demand_keys_touched: self.request_target_demand_keys_touched.get(),
            request_target_candidates_examined: self.request_target_candidates_examined.get(),
            request_claim_entries_examined: self.request_claim_entries_examined.get(),
            request_owner_entries_examined: self.request_owner_entries_examined.get(),
            request_claim_transfer_attempts: self.request_claim_transfer_attempts.get(),
            request_claim_transfer_claims_attempted: self
                .request_claim_transfer_claims_attempted
                .get(),
            request_claim_transfer_commits: self.request_claim_transfer_commits.get(),
            request_claim_transfer_failures: self.request_claim_transfer_failures.get(),
            diagnostic_snapshots_built: self.diagnostic_snapshots_built.get(),
            cohort_compiles: router.cohort_compiles,
            incumbent_active_entries_visited: router.incumbent_active_entries_visited,
            incumbent_plan_requests_visited: router.incumbent_plan_requests_visited,
            incumbent_limited_entries_visited: router.incumbent_limited_entries_visited,
            incumbent_refusal_entries_visited: router.incumbent_refusal_entries_visited,
            active_entries_appended: router.active_entries_appended,
            request_edges_appended: router.request_edges_appended,
            metadata_entries_examined: router.metadata_entries_examined,
        }
    }

    #[doc(hidden)]
    pub fn bench_reset_freshness_work(&self) {
        self.freshness_candidate_atoms.set(0);
        self.freshness_incumbent_demand_edges_visited.set(0);
        self.freshness_plan_request_entries_visited.set(0);
        self.freshness_coalesce_pair_attempts.set(0);
    }

    #[doc(hidden)]
    pub fn bench_freshness_work(&self) -> CoreFreshnessWork {
        CoreFreshnessWork {
            candidate_atoms: self.freshness_candidate_atoms.get(),
            incumbent_demand_edges_visited: self.freshness_incumbent_demand_edges_visited.get(),
            plan_request_entries_visited: self.freshness_plan_request_entries_visited.get(),
            coalesce_pair_attempts: self.freshness_coalesce_pair_attempts.get(),
        }
    }

    /// Reset exact delta-withdrawal counters independently of projection and
    /// storage work.
    #[doc(hidden)]
    pub fn bench_reset_withdrawal_work(&mut self) {
        self.withdrawal_handle_detaches.set(0);
        self.resolver_delta_ops_consumed.set(0);
        self.resolver_owner_keys_touched.set(0);
        self.resolver_surviving_atoms_examined.set(0);
        self.pending_atoms_rebuilt.set(0);
        self.pending_cohort_atoms_reconciled.set(0);
        self.attribution_atoms_rebuilt.set(0);
        self.evidence_candidates_examined.set(0);
        self.diagnostic_snapshots_built.set(0);
        self.nip77_plan_children_touched.set(0);
        self.routing_evidence_owner_keys_touched.set(0);
        self.router.reset_withdrawal_work();
    }

    #[doc(hidden)]
    pub fn bench_withdrawal_work(&self) -> CoreWithdrawalWork {
        let router = self.router.withdrawal_work();
        CoreWithdrawalWork {
            handles_detached: self.withdrawal_handle_detaches.get(),
            resolver_delta_ops_consumed: self.resolver_delta_ops_consumed.get(),
            resolver_owner_keys_touched: self.resolver_owner_keys_touched.get(),
            resolver_surviving_atoms_examined: self.resolver_surviving_atoms_examined.get(),
            pending_atoms_rebuilt: self.pending_atoms_rebuilt.get(),
            evidence_candidates_examined: self.evidence_candidates_examined.get(),
            routing_evidence_owner_keys_touched: self.routing_evidence_owner_keys_touched.get(),
            diagnostic_snapshots_built: self.diagnostic_snapshots_built.get(),
            exact_atoms_closed: router.dropped_atoms,
            request_edges_touched: router.request_edges_touched,
            plan_request_entries_visited: router.plan_request_entries_visited,
            requests_closed: router.requests_closed,
            physical_coverage_edges_released: router.physical_coverage_edges_released,
            diagnostic_refreshes: router.diagnostic_rebuilds,
            diagnostic_requests_visited: router.diagnostic_requests_visited,
            nip77_plan_children_touched: self.nip77_plan_children_touched.get(),
        }
    }

    /// Benchmark-only access to the store work counters used by the
    /// million-row scale proofs. Not an application/store API.
    #[doc(hidden)]
    pub fn bench_reset_query_work(&self) {
        self.store.reset_query_work();
    }

    #[doc(hidden)]
    pub fn bench_query_work(&self) -> (u64, u64, u64) {
        self.store.query_work()
    }

    /// Coverage-table point reads are counted separately from event
    /// projection rows because diagnostics and freshness evidence use them.
    #[doc(hidden)]
    pub fn bench_reset_coverage_reads(&self) {
        self.store.reset_coverage_reads();
    }

    #[doc(hidden)]
    pub fn bench_coverage_reads(&self) -> u64 {
        self.store.coverage_reads()
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
            .ingest_observed_detailed(&mut self.store, events)
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
            .accept_local(&mut self.store, accept)
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
            .accept_local(&mut self.store, accept)
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
        let expired = self.store.expire_due(now).expect("benchmark expiry commit");
        assert_eq!(expired.len(), 1, "benchmark owns exactly one due row");
        let removed = expired.into_iter().map(|row| row.event).collect();
        let committed = self
            .resolver
            .retract(&self.store, removed)
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
