//! The synchronous reducer and durable-state owner (plan §2 position 1,
//! §3.4). `CoreState` holds the concrete `RedbStore`, the M1 resolver
//! `Engine`, the M2 `Router`, the write-delivery state, and the
//! coverage-attribution bookkeeping (`attribution.rs`, `evidence.rs`).
//!
//! **Everything outside `core` talks to [`EngineCore`] (`cell.rs`), never to
//! `CoreState`.** `EngineCore` is a shell holding one private `CoreState`;
//! every mutating door on it proves owner consistency afterwards, which is
//! what makes the proof a property of the reducer rather than of one
//! entrypoint. `CoreState`'s own doors are all `pub(in crate::core)`, so
//! outside this module the type has no callable member at all. The
//! message-driven surface is:
//!
//! ```ignore
//! impl EngineCore {
//!     pub fn handle(&mut self, msg: EngineMsg) -> Vec<Effect>;
//!     pub fn tick(&mut self, now: nostr::Timestamp) -> Vec<Effect>;
//!     pub fn next_deadline(&self) -> Option<nostr::Timestamp>;
//! }
//! ```
//!
//! `CoreState` performs synchronous durable I/O through its `RedbStore`, but
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

mod attribution;
mod author_route_needs;
mod author_route_provider;
pub use author_route_provider::{AuthorRouteProvider, AuthorRouteUpdate, ProviderReroot};
mod auth_transport;
mod cell;
pub use cell::EngineCore;
mod coordinate_coverage;
mod diagnostics;
mod evidence;
mod history;
mod history_lifecycle;
mod lane_projection;
mod observation;
mod owner_index;
mod pending_writes;
mod query;
mod request_attempt;
mod request_effects;
mod request_replacements;
mod request_targets;
mod semantic_delivery;
mod stalled_write_census;
mod wire_ownership;
mod write;
use write::public_auth_denial_source;
pub use write::{PreparedReplaceableMaterialization, PublishPreparation};

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use nostr::{
    filter::MatchEventOptions, Event as SignedEvent, EventBuilder as NostrEventBuilder, EventId,
    PublicKey, RelayMessage, RelayUrl, Timestamp, UnsignedEvent,
};

use nmp_grammar::{
    CacheMode, ConcreteFilter, ContextualAtom, DemandDelta, DemandOp,
    DescriptorHash, Freshness, Identity, LiveQuery, ReadRouting, RelaySessionKey,
    ReplaceableMaterializerOperation, ReplaceableMaterializerRegistration, WriteIntent,
    WritePayload, WriteRouting,
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
    CompensateOutcome, CoverageInterval, CoverageKey, HandoffEvidence, IntentId,
    IntentSigState, MaterializationCandidate, PendingMaterializationState, PersistenceError,
    PromoteOutcome, PromotionTarget, PublishQueueAttemptHandoff,
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
/// Why an attempt was replaced without ever having been acknowledged or
/// refused: nothing is left in this process that could deliver its transport
/// handoff, so the attempt is abandoned in favour of a fresh one rather than
/// held open against a reply that cannot come (#1316).
const ORPHANED_HANDOFF_DETAIL: &str =
    "no transport handoff is outstanding for this attempt; the identical frozen event is \
     republished under a new attempt";
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

use attribution::{AttributionSendId, AttributionState, CompletedAttribution};
use author_route_needs::AuthorRouteNeeds;
pub use diagnostics::{
    AuthDiagnosticsPhase, AuthDiagnosticsSnapshot, DiagnosticsSnapshot, FilterCoverageEntry,
    RelayDiagnosticsSnapshot, StalledWrite, StalledWriteStage, StalledWriteTotals,
};
pub use evidence::{AcquisitionEvidence, AuthPhase, ShortfallFact, SourceEvidence, SourceStatus};
pub use history::{HistoryAdvanceError, HistoryBatch, HistoryQuery, HistorySessionId, WindowLoad};
use history_lifecycle::{HistoryRows, HistorySessions};
use observation::{ActiveRequestEvidence, LiveWireRequest, PendingRequestEvidence};
use pending_writes::PendingWrites;
pub use request_attempt::{LocalSendRefusal, RequestAttemptId, RequestHandoffOutcome};
use request_attempt::{RequestAttemptState, RequestAttempts, RequestSend};
pub use request_effects::{AttemptedReplay, AttemptedWireDelta};
use request_replacements::RequestReplacements;
use request_targets::{ActiveRequestTarget, RequestTargets};
use stalled_write_census::{StalledWriteCensus, StalledWriteInputs};
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
/// terminal, so `CoreState::on_auth_send_completed` re-derives the awaiting
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
    /// `CoreState::on_event_handoff`'s doc for what this does and does
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

#[cfg(any(test, feature = "test-instrumentation"))]
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
    /// provider is still needed. Live `Auto` reads retain authors
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
    /// One observation's merged row transition plus its per-BRANCH
    /// acquisition evidence, indexed by canonical branch order (#1108). A
    /// single-branch live query carries exactly one entry; nothing here is
    /// ever rolled up into a global verdict across branches.
    EmitRows(ObservationId, Vec<RowDelta>, Vec<AcquisitionEvidence>),
    /// One REQ this observation owns reached NIP-01's end of stored events on
    /// this relay, with trustworthy settlement evidence.
    ///
    /// The one execution fact anything outside the engine reads: an
    /// [`AuthorRouteProvider`] learns its own source answered, which is what
    /// turns "no relay list seen" into a settled absence rather than a
    /// silence. It goes to the provider bound to this observation and stops
    /// there -- it never rides the row mailbox.
    RequestSettled(ObservationId, RelayUrl),
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
    /// Some(signing pubkey))` — never the relay's Public
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

/// Per-handle bookkeeping `CoreState` must retain across `handle()` calls:
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
    /// The bounded canonical payload set and its canonical newest-first
    /// order, as one owned fact ([`history_lifecycle::HistoryRows`], #1921).
    /// The two used to be separate fields here, hand-paired by `CoreState`
    /// at thirteen sites.
    rows: HistoryRows,
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
/// cleanup and `nonterminal` answers steady-state worker demand. Every entry
/// here is an exact committed post-state: a transition that did not commit
/// changes nothing, because the store is what recovery reads.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct LaneWorkerProjection {
    persisted: BTreeSet<RelayUrl>,
    nonterminal: BTreeSet<RelayUrl>,
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
        if matches!(lane.state, PublishQueueLaneState::Terminal { .. }) {
            self.nonterminal.remove(&relay);
            self.current_nonterminal.remove(&relay);
        } else {
            self.nonterminal.insert(relay.clone());
            self.current_nonterminal.insert(relay, lane.clone());
        }
        newly_persisted
    }

    fn required_relays(&self) -> impl Iterator<Item = &RelayUrl> {
        self.nonterminal.iter()
    }

    fn can_close(&self) -> bool {
        !self.persisted.is_empty() && self.nonterminal.is_empty()
    }
}

/// Capped exponential backoff, the same shape as [`retry_delay_secs`]
/// without its per-lane jitter: the callers below have exactly one attempt in
/// flight per subject, so there is no thundering herd across ordinals to
/// spread.
fn unjittered_retry_delay_secs(failures: u32) -> u64 {
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
    /// `PublishEvent` (no blind retry, the `AtMostOnce` amendment).
    pending_relays: BTreeSet<RelayUrl>,
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
    /// The authors whose route provider this intent's last resolution still
    /// needs. This includes ordinary `Unknown` inputs and, when the complete
    /// answer has zero destinations, settled zero-route inputs that must keep
    /// discovery alive for a later positive replacement. Unioned into the
    /// protocol-neutral needs set; re-derived on every resolution, never
    /// persisted and never recovered.
    route_needs: BTreeSet<PublicKey>,
}

/// Current logical ownership attached to one immutable router-plan request.
///
/// A later byte-identical router metadata update mutates this one record.
#[derive(Clone)]
struct PlanExecutionMetadata {
    filter: ConcreteFilter,
    coverage_claims: BTreeSet<CoverageKey>,
    owner_demands: BTreeSet<nmp_router::DemandKey>,
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
    /// AUTH is terminally refused for this epoch. The refusal CARRIES who
    /// refused and why: an app whose reads are blocked cannot act on
    /// "denied", and the three causes want three different actions -- its
    /// own policy said no, its own signer said no, or the relay said no.
    /// The write plane has told applications these apart since #756
    /// ([`crate::publish_queue::AuthDenialSource`]); this is the read plane
    /// carrying the same fact.
    Denied {
        source: StoredAuthDenialSource,
        reason: String,
    },
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
    claims: BTreeSet<CoverageKey>,
    due: Timestamp,
    failures: u32,
}

/// The whole live reducer state, and the transitions over it (§2 position 1).
/// No threads.
///
/// **Crate-private, and reachable only through [`EngineCore`]**, which is the
/// one checked door into it: every externally initiated transition runs
/// through `EngineCore::checked`, which proves the mirrored indexes still
/// agree afterwards. Nothing outside `core` can name this type, obtain a
/// reference to one, or call a door on it.
///
/// This is temporary scaffolding that SHRINKS. Every owner extracted from
/// here (`RequestTargets`, `WireOwnership`, `HistorySessions`,
/// `RequestAttempts`, `AuthorRouteNeeds`, `PendingWrites` so
/// far) removes fields and decisions from it. It is not the semantic owner of engine
/// state and must not become one -- that is the god object this
/// decomposition exists to dissolve.
#[doc(hidden)]
pub struct CoreState {
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
    /// Live-wire owner count per author contributed by `Auto` atoms,
    /// which authors still lack a positive outbound route, and the
    /// pending-change flag for `AuthorRouteNeedsChanged`. Private to
    /// `author_route_needs.rs`, so the incremental and wholesale-rebuild
    /// paths share one algorithm instead of two that can drift.
    author_outbox_route_needs: AuthorRouteNeeds,
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
    /// (#1606 step 1). Its maps are private to `request_attempt.rs`: privacy
    /// is compiler-enforced, but the reverse-index invariants over those
    /// maps are not -- they are enforced by asserts in
    /// `RequestAttempts::remove`/`remove_retry` and the owner-scoped bulk
    /// removals that call them, and checked structurally by
    /// `RequestAttempts::assert_consistent` (wired into
    /// `assert_owner_consistency` below).
    attempts: RequestAttempts,
    /// Every accepted-open-before-close transition still waiting on its
    /// successor's admission, keyed by successor and mirrored by owning
    /// session (#774, #1606). Private maps in `request_replacements.rs`,
    /// reusing the same mirrored-index mechanism.
    request_replacements: RequestReplacements,
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
    /// CoreState's memory of the exact connection generation and SESSION
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
    /// each PROTECTED session (#8). session bound to no identitys never enter this map. A
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
    #[cfg(feature = "test-instrumentation")]
    maintenance_turns: u64,
    active_pubkey: Option<PublicKey>,
    /// Every open durable write obligation and the three indexes that mirror
    /// it (§3.4 / VISION §7, guarantee #6/#9). Its five fields are private to
    /// `pending_writes.rs`, so "which receipt owns this intent", "which
    /// receipts own these bytes" and "which receipts have a lane on this
    /// relay" have one implementation each instead of a door plus the
    /// hand-written spellings that kept drifting from it (#1606).
    pending: PendingWrites,
    /// Last complete neutral author-route provider-work set published to the
    /// optional protocol assembly. The set is the union of unresolved write
    /// contributors and authors in live `Auto` reads without a
    /// positive outbound route. Keeping the prior value here makes provider
    /// synchronization an edge rather than a repeated side effect of every
    /// unrelated recompile.
    ///
    /// Deliberately stays here rather than moving into `AuthorRouteNeeds`:
    /// it is the last-published snapshot of `author_route_needs()`'s UNION,
    /// which mixes write-plane state (`pending`'s `route_needs`, refreshed
    /// per intent by the route resolver in `write.rs`) with this owner's own
    /// `needs`. `AuthorRouteNeeds` deliberately knows nothing about `pending`
    /// or write intents -- giving it this field would mean giving it that
    /// visibility too, trading one coordinator-level field for a real
    /// boundary violation. The root composition/order state this field
    /// represents (the last thing told to an outside consumer, unioning two
    /// owners' contributions) belongs with the coordinator that computes the
    /// union, not with either half of it.
    last_author_route_needs: BTreeSet<PublicKey>,
    /// Which open obligations are stuck and the bounded projection of them
    /// diagnostics snapshots carry (#1743). Its three fields are private to
    /// `stalled_write_census.rs`, so the cache can never drift from the
    /// census it is a projection of.
    stalled_writes: StalledWriteCensus,
    /// Latest provenance-bearing NIP-11 advertisement for relays in the
    /// current read plan. Recompile pruning and completion-time plan checks
    /// prevent historical relay churn from becoming a shadow cache.
    nip11_information: HashMap<RelayUrl, RelayInformationCapabilityEvidence>,
    /// Exact shadow of router sessions used by incremental plan housekeeping.
    planned_read_sessions: BTreeSet<RelaySessionKey>,
    planned_read_session_counts_by_relay: BTreeMap<RelayUrl, usize>,
    /// Router plan request -> current logical metadata for that immutable
    /// physical request.
    plan_execution_metadata: HashMap<SubId, PlanExecutionMetadata>,
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
    /// Runtime relay-worker open failures keyed by their exact current owner.
    /// Entries are pruned whenever demand/write ownership changes and cleared
    /// by a successful connection for that session.
    relay_open_failures: BTreeMap<RelaySessionKey, String>,
    /// Transport health/verifier degradation from a live worker. Kept
    /// separate from open failures so clearing one recovered session cannot
    /// erase an independent transport-health fact.
    transport_degraded: Option<String>,
    /// The attempt ceiling (#1031), from
    /// `nmp::EngineConfig::max_publish_attempts`. Counts
    /// observations, never wall-clock.
    max_publish_attempts: u64,
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

impl CoreState {
    pub(in crate::core) fn install_replaceable_materializer(
        &mut self,
        registration: ReplaceableMaterializerRegistration,
    ) {
        self.replaceable_materializers
            .insert((registration.program, registration.format), registration);
    }

    pub(in crate::core) fn install_replaceable_materializers(
        &mut self,
        capabilities: Vec<nmp_grammar::ReplaceableMaterializerSpec>,
    ) {
        for spec in capabilities {
            self.install_replaceable_materializer(spec.into_registration());
        }
    }

    pub(in crate::core) fn new(store: RedbStore, cap: usize) -> Self {
        Self::new_with_routing_facts(store, RoutingFactStore::default(), cap)
    }

    pub(in crate::core) fn new_with_routing_facts(
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
            author_outbox_route_needs: AuthorRouteNeeds::default(),
            observations: HashMap::new(),
            next_observation_id: 0,
            history: HistorySessions::new(),
            attribution: AttributionState::new(),
            pending_request_evidence: HashMap::new(),
            attempts: RequestAttempts::new(),
            request_replacements: RequestReplacements::default(),
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
            #[cfg(feature = "test-instrumentation")]
            maintenance_turns: 0,
            active_pubkey: None,
            pending: PendingWrites::default(),
            last_author_route_needs: BTreeSet::new(),
            stalled_writes: StalledWriteCensus::default(),
            nip11_information: HashMap::new(),
            planned_read_sessions: BTreeSet::new(),
            planned_read_session_counts_by_relay: BTreeMap::new(),
            plan_execution_metadata: HashMap::new(),
            events_by_session_kind: HashMap::new(),
            next_attempt_correlation: Some(0),
            attempt_correlations: HashMap::new(),
            relay_open_failures: BTreeMap::new(),
            transport_degraded: None,
            max_publish_attempts: crate::publish_queue::DEFAULT_MAX_PUBLISH_ATTEMPTS,
        }
    }

    /// The sole neutral author-route mutation door. Replacement and the
    /// resulting Auto-write wake happen in one reducer turn.
    #[allow(dead_code)]
    pub(in crate::core) fn replace_author_routes(
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
    pub(in crate::core) fn with_max_publish_attempts(mut self, max_publish_attempts: u64) -> Self {
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
    /// word for anything in `CoreState`, which owns the store and commits
    /// through it -- see `docs/internals/architecture-boundaries.md`.)
    pub(in crate::core) fn relay_worker_requirements(&self) -> RelayWorkerRequirements {
        let writes = self.write_relay_workers();
        let mut all: BTreeSet<RelaySessionKey> = self.router.plan().reqs.keys().cloned().collect();
        all.extend(writes.iter().cloned());
        RelayWorkerRequirements { all, writes }
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
            let access = Some(pending.signing_pubkey);
            required.extend(
                pending
                    .pending_relays
                    .iter()
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
    /// non-default `Demand`. #107's `ReadRouting::Explicit` is the first
    /// production path that does, so a reconstruction would silently
    /// collapse two genuinely-distinct atoms (same selection, different
    /// context) that the resolver correctly tracks as two independent
    /// entries into one. Widened rather than patched with an assertion, and
    /// no alias kept for the old spelling -- this mirrors
    /// `nmp_resolver::Engine::active_demand()` exactly.
    pub(in crate::core) fn active_demand(&self) -> BTreeSet<ContextualAtom> {
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
    ///
    /// Covers every extracted owner. The list is the body below, not a
    /// sentence here: an enumeration in prose is a hand-maintained mirror of
    /// a function's call sites, and this one had already drifted twice — it
    /// said "six" when seven were covered (#1759), was corrected to "seven",
    /// and was stale again two commits later when `author_outbox_route_needs`
    /// was added. A campaign against hand-maintained mirrors should not
    /// carry one in its own doc comment.
    ///
    /// Not covered, and known: (`states`/`pending` have a real
    /// cross-map invariant) and `auth_ready_sessions` mirroring
    /// `auth_sessions[s].phase == Ready`. Both are live mirrors outside this
    /// check. `AttributionState` was a third such omission until #1850 — the
    /// owner with the most test reach-through in the crate was the one owner
    /// with no consistency proof at all, and its absence was not written down
    /// here either.
    #[cfg(any(test, feature = "test-instrumentation"))]
    pub(in crate::core) fn assert_owner_consistency(&self, at: &str) {
        self.attribution.assert_consistent(at);
        self.wire.assert_consistent(at);
        self.author_outbox_route_needs.assert_consistent(at);
        self.request_targets.assert_consistent(at);
        self.request_replacements.assert_consistent(at);
        self.attempts.assert_consistent(at);
        self.pending.assert_consistent(at);
        self.stalled_writes.assert_consistent(
            at,
            StalledWriteInputs {
                pending: &self.pending,
                connected: &self.connected_relays,
            },
        );
        self.history.assert_consistent(at);
    }

    #[cfg(any(test, feature = "test-instrumentation"))]
    pub(in crate::core) fn observation_ownership_census(&self) -> CoreObservationOwnershipCensus {
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
    /// `ConcreteFilter`-only signature reconstructed `source`/`access` by
    /// inspecting whether the filter bound `authors`, which was exact only
    /// as long as every production atom took that one path; #107's
    /// `ReadRouting::Explicit` breaks that assumption; the reconstruction
    /// would then compute the WRONG `CoverageKey` and silently report
    /// "not covered" for coverage that IS actually proven.
    pub(in crate::core) fn get_coverage(
        &self,
        atom: &ContextualAtom,
        session: &RelaySessionKey,
    ) -> Result<Option<nmp_store::CoverageInterval>, PersistenceError> {
        self.store
            .get_coverage(nmp_store::coverage_key(atom), session)
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
    pub(in crate::core) fn diagnostics_snapshot(&self) -> DiagnosticsSnapshot {
        let mut snapshot = diagnostics::build(
            self.router.diagnostics(),
            self.router.plan(),
            &self.events_by_session_kind,
            |session: &RelaySessionKey, key| self.store.get_coverage(key, session),
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
        snapshot.transport_degraded = self
            .relay_open_failures
            .iter()
            .next()
            .map(|(session, reason)| format!("{}: {reason}", session.relay))
            .or_else(|| self.transport_degraded.clone());
        let mut auth_sessions = BTreeMap::new();
        for (handle, session) in self.slot_to_relay.values() {
            if session.authenticate_as.is_none() || !self.connected_relays.contains(session) {
                continue;
            }
            auth_sessions.insert(
                session.clone(),
                AuthDiagnosticsSnapshot {
                    relay: session.relay.clone(),
                    authenticate_as: session.authenticate_as,
                    transport_slot: handle.slot,
                    transport_generation: handle.generation,
                    epoch_sequence: None,
                    challenge_hash: None,
                    phase: AuthDiagnosticsPhase::AwaitingChallenge,
                    policy_bound: false,
                    signer_bound: false,
                    auth_event_id: None,
                    denial_source: None,
                    denial_reason: None,
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
                AuthSessionPhase::Denied { .. } => (AuthDiagnosticsPhase::Denied, None),
                AuthSessionPhase::Error => (AuthDiagnosticsPhase::Error, None),
            };
            let denial = match &state.phase {
                AuthSessionPhase::Denied { source, reason } => {
                    Some((public_auth_denial_source(*source), reason.clone()))
                }
                _ => None,
            };
            auth_sessions.insert(
                session.clone(),
                AuthDiagnosticsSnapshot {
                    relay: session.relay.clone(),
                    authenticate_as: session.authenticate_as,
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
                    denial_source: denial.as_ref().map(|(source, _)| *source),
                    denial_reason: denial.map(|(_, reason)| reason),
                },
            );
        }
        snapshot.auth_sessions = auth_sessions.into_values().collect();
        snapshot.stalled_writes = self.stalled_writes.rows().to_vec();
        snapshot.stalled_write_totals = self.stalled_writes.totals();
        for relay in &mut snapshot.relays {
            // NIP-11 advertisement is earned on the connection bound to no
            // identity (#8): the one-shot HTTP document runs outside that
            // socket, so an identity-bound session's row must never inherit
            // them — its capability facts stay honestly "unknown".
            if relay.authenticate_as.is_some() {
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
        }
        snapshot
    }

    /// A pure clock update plus the owned deadline sweeps: failed
    /// post-settlement request-claim transfers and NIP-40 expiry
    /// (retraction-and-negative-deltas.md §3.2 — drains `store.expire_due`
    /// and retracts every row past its deadline).
    /// Claim-transfer retry records retain the exact request revision,
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
    pub(in crate::core) fn tick(&mut self, now: Timestamp) -> Vec<Effect> {
        #[cfg(feature = "test-instrumentation")]
        {
            self.maintenance_turns = self.maintenance_turns.saturating_add(1);
        }
        self.clock = now;
        let mut effects = Vec::new();
        self.retry_due_request_claim_transfers(now, &mut effects);
        self.retry_due_request_attempts(now, &mut effects);
        // Before the durable deadline sweep: a committed bootstrap mints the
        // very lanes the sweep and `schedule_ready` below then act on, so
        // retrying first lets one tick both close the projection gap and
        // make progress on it.
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
                match self.resolver.retract(&self.store, Vec::new(), removed) {
                    Ok(committed) => {
                        self.apply_committed_mutation(committed, &mut effects);
                    }
                    Err(_e) => {},
                }
            }
            Ok(_) => {}
            Err(_e) => {},
        }
        effects
    }

    #[cfg(feature = "test-instrumentation")]
    pub(in crate::core) fn maintenance_turn_count(&self) -> u64 {
        self.maintenance_turns
    }

    /// Advance reducer wall-clock truth without executing any deadline work.
    /// Runtime does this once at command boundaries; due expiry, retry, and
    /// liveness work remain exclusively owned by [`Self::tick`] and
    /// [`Self::next_deadline`].
    pub(in crate::core) fn advance_clock(&mut self, now: Timestamp) {
        self.clock = now;
    }

    /// The reducer's own current wall truth. Effect dispatch opens the NIP-65
    /// route-source observation with this rather than re-reading a clock the
    /// reducer has not seen yet -- the same value [`Self::on_subscribe`] uses
    /// for an app subscription.
    pub(in crate::core) fn clock(&self) -> Timestamp {
        self.clock
    }

    /// The earliest wall-clock instant at which [`Self::tick`] must run for
    /// something to actually happen (retraction-and-negative-deltas.md
    /// §3.2): the min over every deadline source this reducer currently
    /// tracks -- NIP-40 expiry (`store.next_expiration()`, index-backed)
    /// and request-claim transfer backoff.
    /// `None` means no timer needs to fire at
    /// all right now: `runtime::engine_loop`'s `recv_timeout` driver (§3.3)
    /// sleeps forever on the plain `recv()` in that case, exactly matching
    /// the doc's "a light embedder with no deadlines pays nothing".
    /// Extensible to future timers (drop-grace debounce) by folding
    /// another `.min()` term in here -- the runtime driver itself never
    /// needs to change to pick up a new deadline source.
    ///
    /// The two durable terms are index reads that can fail. A failed read is
    /// folded in here as "this term names no deadline", because that is the
    /// only thing any caller ever did with the failure: the store is a cache
    /// of durable rows, not the engine's memory, and a read that could not
    /// answer costs this pass only -- the next message re-reads it, and the
    /// durable rows are untouched either way. Nothing is lost that the next
    /// boot's recovery does not rebuild from those same rows.
    pub(in crate::core) fn next_deadline(&self) -> Option<Timestamp> {
        let expiry = self.store.next_expiration().ok().flatten();
        let delivery = self.store.next_publish_queue_deadline().ok().flatten();
        let request_claim_transfer = self
            .pending_request_claim_transfers
            .values()
            .map(|pending| pending.due)
            .min();
        let request_retry = self.attempts.next_retry_due();
        [
            expiry,
            delivery,
            request_claim_transfer,
            request_retry,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    pub(in crate::core) fn handle(&mut self, msg: EngineMsg) -> Vec<Effect> {
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
                if self.relay_worker_requirements().all.contains(&session) {
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
        // No owner-consistency check here. It used to live at this one site,
        // which meant `handle()` was checked and the other ~37 externally
        // reachable `&mut self` doors were not -- `cancel_write` was checked
        // through `EngineMsg::CancelWrite` and unchecked when the runtime
        // called it directly. The check now runs in `EngineCore::checked`
        // (`cell.rs`), so it covers every door instead of this one.
        effects
    }

    fn prune_unowned_relay_state(&mut self) -> bool {
        if self.relay_open_failures.is_empty() && self.auth_required_sessions.is_empty() {
            return false;
        }
        let required = self.relay_worker_requirements();
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
    /// because `CoreState` is exercised headlessly with no runtime in
    /// existence at all. `RuntimeSessionState` therefore keeps no second
    /// copy: it owns the account set and asks here for the selection.
    pub(in crate::core) fn active_pubkey(&self) -> Option<PublicKey> {
        self.active_pubkey
    }

    fn on_set_active_pubkey(&mut self, pk: Option<PublicKey>) -> Vec<Effect> {
        self.active_pubkey = pk;
        let mut effects = Vec::new();
        // Re-rooting reactive nodes can re-query the store (a `Derived`
        // binding over a reactive field). Degrade to read-only on a
        // persistence failure (issue #122) rather than panic.
        if let Err(_e) = self.resolver.set_active_pubkey(&self.store, pk) {
            return effects;
        }
        let ids: Vec<_> = self.handles.keys().copied().collect();
        for id in ids {
            self.reconcile_observation_resolution(id);
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

#[cfg(feature = "test-instrumentation")]
impl CoreState {
    /// Test-only access to the concrete store-door count used by the relay
    /// worker scheduling falsifiers.
    #[doc(hidden)]
    pub(in crate::core) fn reset_publish_queue_lane_recovery_reads(&self) {
        self.store.reset_publish_queue_lane_recovery_reads();
    }

    #[doc(hidden)]
    pub(in crate::core) fn publish_queue_lane_recovery_reads(&self) -> u64 {
        self.store.publish_queue_lane_recovery_reads()
    }

    /// Seeds a stale `relay_open_failures`/`auth_required_sessions` entry
    /// for `session` -- the same shape `EngineMsg::RelayOpenFailed` leaves
    /// once `session` stops being required (#1803 falsifier support).
    ///
    /// `handle`'s epilogue (`prune_unowned_relay_state`) runs after EVERY
    /// message, so whichever call first observes a session drop out of
    /// `relay_worker_requirements()` is the one credited with the cleanup
    /// effect -- an ordinary sequence of `handle()` calls can never leave
    /// this state stale FOR a specific later message to discover, because
    /// the call that causes the drop always sees it first, in its own
    /// return. This door seeds the staleness directly, before the first
    /// `handle()` call this session has ever seen, so a caller in another
    /// crate can drive the exact turn under test deterministically instead
    /// of needing a live relay connection and AUTH handshake whose own
    /// turn would otherwise claim the credit.
    #[doc(hidden)]
    pub(in crate::core) fn seed_stale_relay_open_failure_for_test(
        &mut self,
        session: RelaySessionKey,
        reason: String,
    ) {
        self.relay_open_failures.insert(session.clone(), reason);
        self.auth_required_sessions.insert(session);
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

