//! The generational WebSocket `Pool` (M3 plan §3.2). HARVEST target: the
//! old repo's `mio`-driven worker-thread pool
//! (`crates/nmp-network/src/pool/{mod,types,inner}.rs`,
//! `relay_worker/{connect,socket_io,mod}.rs`, `relay_protocol.rs`,
//! `keepalive.rs`) — generational handles, push-model (no `send_to_all`),
//! backoff+jitter constants, keepalive FSM, and the reconnect-preamble
//! replay hook are operational lessons re-earned, not re-invented (plan
//! §4). The `PoolEvent` <-> `EngineMsg` translation is fresh — that glue
//! lives in `nmp-engine::runtime`, not here.
//!
//! A2: `Pool` is a thin, cheap-to-clone facade (`Arc<Mutex<PoolInner>>`)
//! over [`pool::inner::PoolInner`] + [`pool::worker`]'s per-relay `mio`
//! thread. See those modules' docs for the generation-safety scheme and the
//! harvest-vs-rewrite breakdown.

#[cfg(feature = "bench-instrumentation")]
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub use nmp_grammar::RelaySessionKey;
use nostr::{Event, EventId, JsonUtil, RelayMessage, RelayUrl, SubscriptionId};

use crate::handle::RelayHandle;
use crate::health::RelayHealth;

mod committed_observations;
mod connect;
mod frame;
mod inner;
mod spawn;
mod verify;
mod worker;

pub use committed_observations::{
    CommittedObservationCandidate, CommittedObservationHit, CommittedObservationPublication,
};
use inner::PoolInner;
use spawn::{system_spawner, ThreadSpawner};
pub use verify::{
    KnownSig, NullKnownSig, Verdict, Verifier, VerifyConfig, DEFAULT_VERIFIER_WORKERS,
    MAX_DEFAULT_VERIFIER_WORKERS, MAX_VERIFIER_WORKERS,
};
pub use worker::ReconnectPreambleTransition;

/// The finite thread role whose OS spawn was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadRole {
    RelayWorker,
    RetirementReaper,
    PoolTranslator,
    VerifierWorker,
}

impl std::fmt::Display for ThreadRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::RelayWorker => "relay worker",
            Self::RetirementReaper => "relay retirement reaper",
            Self::PoolTranslator => "pool translator",
            Self::VerifierWorker => "signature verifier",
        })
    }
}

/// Safe, owned description of an OS thread-spawn refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSpawnError {
    pub role: ThreadRole,
    pub reason: String,
}

impl std::fmt::Display for ThreadSpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} thread unavailable: {}", self.role, self.reason)
    }
}

impl std::error::Error for ThreadSpawnError {}

#[cfg(feature = "bench-instrumentation")]
pub fn configure_diagnostic_duplicate_ceiling(capacity: usize, event_payload_only: bool) {
    frame::configure_diagnostic_duplicate_ceiling(capacity, event_payload_only);
}

#[cfg(feature = "bench-instrumentation")]
#[doc(hidden)]
pub fn configure_diagnostic_preparsed_ceiling(
    subscription_id: Option<SubscriptionId>,
    events: Vec<Arc<Event>>,
) {
    frame::configure_diagnostic_preparsed_ceiling(subscription_id, events);
}

/// Safe default for the single engine/transport relay ceiling. Zero is
/// normalized to this value as well, so legacy/default construction cannot
/// silently re-enable unbounded worker growth.
pub const DEFAULT_MAX_RELAYS: usize = 10;

/// A pool cannot exist without its finite verifier/translation/retirement
/// executors. Construction is all-or-nothing and cleans up any threads that
/// were started before a later spawn failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoolBuildError {
    ThreadUnavailable(ThreadSpawnError),
    RelayBudgetOverflow { max_relays: usize },
}

impl std::fmt::Display for PoolBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ThreadUnavailable(error) => error.fmt(f),
            Self::RelayBudgetOverflow { max_relays } => write!(
                f,
                "relay worker budget {max_relays} cannot represent its finite retirement envelope"
            ),
        }
    }
}

impl std::error::Error for PoolBuildError {}

/// A typed refusal to create or recover a relay worker.
///
/// Callers must handle this result before they receive a [`RelayHandle`], so
/// a relay-cap refusal cannot be mistaken for a live generation and silently
/// fed into [`Pool::send`] as an opaque sentinel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayOpenError {
    /// Opening another live worker would exceed the pool-wide ceiling.
    AtCapacity { max_relays: usize },
    /// The pool has entered terminal shutdown and cannot reopen workers.
    ShuttingDown,
    /// Pool state was poisoned; fail closed instead of returning a handle.
    Unavailable,
    /// The OS refused the relay worker thread. No slot or generation was
    /// published and the thread budget remains unchanged.
    ThreadUnavailable(ThreadSpawnError),
}

impl std::fmt::Display for RelayOpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AtCapacity { max_relays } => {
                write!(f, "relay pool capacity {max_relays} exhausted")
            }
            Self::ShuttingDown => f.write_str("relay pool is shutting down"),
            Self::Unavailable => f.write_str("relay pool state is unavailable"),
            Self::ThreadUnavailable(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for RelayOpenError {}

/// A frame handed to the pool for sending. Substrate-grade: no "kind"/
/// "pubkey" here — the pool moves bytes, it never interprets Nostr
/// semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireFrame {
    Text(String),
    Binary(Vec<u8>),
}

/// An opaque correlation token for one durable `EVENT` handoff (issue #93).
/// Transport-native and meaningless to this crate beyond identity — the
/// caller (the engine) mints it from its own persisted attempt bookkeeping
/// (`(IntentId, RelayUrl, ordinal)` in `nmp-store` terms) and maps it back
/// on the way in; this crate never needs to know what it means, only that
/// each one gets EXACTLY one [`HandoffResult`], ever. Kept distinct from a
/// bare `u64` so a caller can't accidentally pass an ordinal, a slot, or any
/// other transport-internal number where a correlation is expected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AttemptCorrelation(pub u64);

/// The one typed result of a durable `EVENT` handoff (issue #93). Exactly
/// three classes — never collapsed to a bool, never silently re-queued past
/// the connection generation it was submitted against:
///
/// - [`Self::NotHandedOff`]: PROVEN the frame never reached a socket write
///   call for this generation — still queued, or the handle/generation was
///   already stale at submission. Safe to resubmit under a fresh generation
///   with no ambiguity about double-delivery.
/// - [`Self::Written`]: PROVEN the socket write AND the subsequent flush
///   both completed before this generation ended. The ONLY result that may
///   later become `Sent` (retraction-and-negative-deltas.md's sibling
///   principle for writes: don't claim delivery you can't back up).
/// - [`Self::Ambiguous`]: UNKNOWN whether the relay received it — a write
///   was accepted by the socket library but its flush was never confirmed
///   before the connection ended (or broke), or the connection died mid
///   in-flight write. Durable durability waits for an ACK/timeout policy
///   (#95); `AtMostOnce` becomes `OutcomeUnknown` — either way, NEVER a
///   blind resend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoffResult {
    NotHandedOff,
    Written,
    Ambiguous,
}

/// Immediate result of submitting one durable EVENT to a relay worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurableSendOutcome {
    Queued,
    Resolved(HandoffResult),
}

/// An opaque correlation token for one exact-generation ephemeral handoff
/// (issue #883). Minted by the caller from its own operation bookkeeping and
/// echoed back verbatim on [`PoolEvent::EphemeralHandoff`]; this crate never
/// interprets it beyond identity. Kept distinct from [`AttemptCorrelation`]
/// so a durable EVENT correlation and a connection-scoped operation can never
/// be passed where the other is expected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EphemeralOperation(pub u64);

/// Terminal result of one exact-generation, nonpersistent frame handoff.
///
/// This lane is for connection-scoped protocol messages whose authority
/// disappears with the current socket generation (for example NIP-42 AUTH).
/// It is intentionally separate from ordinary reconnecting traffic and from
/// durable EVENT correlations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EphemeralSendOutcome {
    /// The frame's socket write and flush completed on the exact requested
    /// session generation.
    Accepted,
    /// No such live connected session existed, the generation changed, or
    /// the generation ended before its write and flush completed.
    Unavailable,
}

/// Immediate disposition of starting an exact-generation ephemeral send.
///
/// The two arms are the whole synchronous-versus-asynchronous distinction:
/// exactly one of them is true for every submitted [`EphemeralOperation`],
/// and nothing else in this crate resolves that operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EphemeralSendStart {
    /// The exact worker now owns the operation and will emit EXACTLY ONE
    /// [`PoolEvent::EphemeralHandoff`] carrying its terminal
    /// [`EphemeralSendOutcome`].
    Pending,
    /// The pool refused the operation synchronously. No worker ever saw it,
    /// so no [`PoolEvent::EphemeralHandoff`] will ever be emitted for this
    /// token — a refusal can never leave an orphan completion behind.
    Resolved(EphemeralSendOutcome),
}

/// One parsed, owned relay message off the wire.
///
/// Ordinary text is parsed exactly once at the transport boundary. EVENT
/// payloads move immediately into an [`Arc`], so signature workers and the
/// engine share the same parsed allocation instead of deep-cloning content
/// and tags. Exact post-commit observations may carry a revalidated preparse
/// token, retaining their raw text for fail-closed ordinary fallback. Keepalive
/// `Ping`/`Pong`, binary messages, and the
/// WebSocket `Close` frame never reach this type — they are consumed by the
/// worker's keepalive FSM / surfaced instead as [`PoolEvent::Disconnected`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayFrame {
    Event {
        subscription_id: SubscriptionId,
        event: Arc<Event>,
        observation_candidate: Option<CommittedObservationCandidate>,
    },
    #[doc(hidden)]
    CommittedObservation(CommittedObservationHit),
    Message(Box<RelayMessage<'static>>),
}

impl RelayFrame {
    /// Wrap an already-owned relay message.
    ///
    /// This is primarily the typed construction door used by headless engine
    /// tests. Live wire input is constructed only by `pool::frame`, after its
    /// single JSON parse.
    #[must_use]
    pub fn from_message(message: RelayMessage<'static>) -> Self {
        match message {
            RelayMessage::Event {
                subscription_id,
                event,
            } => Self::Event {
                subscription_id: subscription_id.into_owned(),
                event: Arc::new(event.into_owned()),
                observation_candidate: None,
            },
            message => Self::Message(Box::new(message)),
        }
    }

    /// Borrow an EVENT payload through its shared parsed allocation.
    #[must_use]
    pub fn event(&self) -> Option<&Arc<Event>> {
        match self {
            Self::Event { event, .. } => Some(event),
            Self::CommittedObservation(_) | Self::Message(_) => None,
        }
    }

    #[cfg(feature = "bench-instrumentation")]
    const DIAGNOSTIC_DUPLICATE_CEILING_MARKER: &'static str = "\0nmp-663-ceiling";

    #[cfg(feature = "bench-instrumentation")]
    pub(crate) fn diagnostic_duplicate_ceiling_token(
        event_kind: u16,
        encoded_bytes: usize,
    ) -> Self {
        let mut encoded = [0_u8; EventId::LEN];
        encoded[..2].copy_from_slice(&event_kind.to_be_bytes());
        encoded[2..10].copy_from_slice(&(encoded_bytes as u64).to_be_bytes());
        Self::Message(Box::new(RelayMessage::Ok {
            event_id: EventId::from_byte_array(encoded),
            status: false,
            message: Cow::Borrowed(Self::DIAGNOSTIC_DUPLICATE_CEILING_MARKER),
        }))
    }

    #[cfg(feature = "bench-instrumentation")]
    #[must_use]
    pub fn diagnostic_duplicate_ceiling(&self) -> Option<(u16, usize)> {
        let Self::Message(message) = self else {
            return None;
        };
        let RelayMessage::Ok {
            event_id,
            status: false,
            message,
        } = message.as_ref()
        else {
            return None;
        };
        if message.as_ref() != Self::DIAGNOSTIC_DUPLICATE_CEILING_MARKER {
            return None;
        }
        let encoded = event_id.as_bytes();
        let event_kind = u16::from_be_bytes(encoded[..2].try_into().ok()?);
        let encoded_bytes = u64::from_be_bytes(encoded[2..10].try_into().ok()?);
        Some((event_kind, usize::try_from(encoded_bytes).ok()?))
    }

    /// Move an EVENT into the engine, normally without cloning.
    ///
    /// The translator drops every temporary verifier reference before sink
    /// delivery, making `Arc::try_unwrap` the production path. The clone is a
    /// defensive fallback for public callers that retained a frame clone.
    // The error intentionally owns the exact raw websocket text needed for a
    // fail-closed cache fallback. Boxing it would allocate on every hit.
    #[allow(clippy::result_large_err)]
    pub fn into_event(self) -> Result<Event, Self> {
        self.into_observed_event().map(|(_, event, _)| event)
    }

    pub(crate) fn from_observed_event(
        subscription_id: SubscriptionId,
        event: Event,
        observation_candidate: CommittedObservationCandidate,
    ) -> Self {
        Self::Event {
            subscription_id,
            event: Arc::new(event),
            observation_candidate: Some(observation_candidate),
        }
    }

    /// Move an EVENT and its preparse cache candidate into the engine.
    // See `into_event`: retaining the allocation-free owned error is a
    // measured hot-path choice, not an accidentally large error payload.
    #[allow(clippy::result_large_err)]
    pub fn into_observed_event(
        self,
    ) -> Result<(SubscriptionId, Event, Option<CommittedObservationCandidate>), Self> {
        match self {
            Self::Event {
                subscription_id,
                event,
                observation_candidate,
            } => Ok((
                subscription_id,
                Arc::try_unwrap(event).unwrap_or_else(|event| {
                    #[cfg(feature = "bench-instrumentation")]
                    crate::ingest_attribution::event_fallback_clone();
                    event.as_ref().clone()
                }),
                observation_candidate,
            )),
            other => Err(other),
        }
    }

    /// Recover the exact ordinary EVENT path after an engine-side lease,
    /// session, or pending-intent revalidation rejects a preparse hit.
    #[doc(hidden)]
    #[must_use]
    pub fn into_ordinary_fallback(self) -> Option<Self> {
        match self {
            Self::CommittedObservation(hit) => {
                let (raw_text, candidate) = hit.into_raw_and_candidate();
                match frame::classify_text_with_candidate(raw_text.as_str(), Some(candidate)) {
                    frame::ClassifiedFrame::Frame(frame) => Some(frame),
                    // This text already decoded once, on the way into the
                    // cache, so a failure here is a local cache/lease fault
                    // rather than relay behavior — and the engine already
                    // erases every count on the session for any committed
                    // observation hit, so nothing is left unreported.
                    frame::ClassifiedFrame::Consumed | frame::ClassifiedFrame::Undecodable => None,
                }
            }
            frame => Some(frame),
        }
    }

    /// Reconstitute the typed relay message. Engine EVENT ingest should prefer
    /// [`Self::into_event`] so its hot path can unwrap the shared allocation.
    #[must_use]
    pub fn into_message(self) -> RelayMessage<'static> {
        match self {
            Self::Event {
                subscription_id,
                event,
                ..
            } => RelayMessage::event(
                subscription_id,
                Arc::try_unwrap(event).unwrap_or_else(|event| {
                    #[cfg(feature = "bench-instrumentation")]
                    crate::ingest_attribution::event_fallback_clone();
                    event.as_ref().clone()
                }),
            ),
            Self::CommittedObservation(hit) => RelayMessage::from_json(hit.raw_text())
                .expect("committed observation retains its previously parsed EVENT bytes"),
            Self::Message(message) => *message,
        }
    }
}

impl From<RelayMessage<'static>> for RelayFrame {
    fn from(message: RelayMessage<'static>) -> Self {
        Self::from_message(message)
    }
}

/// Why a relay slot disconnected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisconnectReason {
    /// `Pool::close` was called for this handle.
    Closed,
    /// A TRANSIENT transport error (dial failure, socket error, peer-
    /// initiated close, keepalive timeout) tore down a previously-`Connected`
    /// session. The pool itself keeps redialing on its own backoff schedule
    /// -- this variant never accompanies a worker retirement. [`Pool::health`]
    /// carries the message and the next retry delay.
    Error,
    /// The relay's own failure was PERMANENT (`backoff::is_permanent_error`
    /// -- HTTP 401/403/Forbidden, i.e. NIP-42-auth-required, IP-banned, or an
    /// expired-paid relay): the worker will never redial on its own. The
    /// pool retires the worker thread and frees its `max_relays` cap slot the
    /// instant this is emitted (both when the slot was previously `Connected`
    /// and when it never got that far) -- there is no lingering zombie
    /// `state.worker` for a caller to get idempotently handed back. Recovery
    /// requires an explicit fresh [`Pool::ensure_open`] after the caller has
    /// addressed the denial (e.g. NIP-42 AUTH); the pool never self-reopens
    /// this slot, which would otherwise busy-loop against a relay that keeps
    /// saying no.
    PermanentlyFailed,
}

/// Events the pool pushes to its [`PoolEventSink`]. Reconnect always mints
/// a NEW generation for the slot (ledger #2/#3/#4) — `Connected` carries
/// the fresh [`RelayHandle`].
#[derive(Debug, Clone)]
pub enum PoolEvent {
    Connected {
        handle: RelayHandle,
        session: RelaySessionKey,
    },
    /// The exact protected connection generation completed its initial
    /// socket observation and final nonblocking read-drain. Any observed
    /// frame precedes this edge in this generation's worker-produced event
    /// subsequence (the retirement reaper is a separate producer). Public
    /// sessions skip this handshake and never emit this marker.
    InitialReadCompleted {
        handle: RelayHandle,
        session: RelaySessionKey,
    },
    Disconnected {
        /// The exact connection generation that disconnected. A slot may
        /// already have reopened by the time this event is reduced, so a
        /// bare slot number cannot safely identify the connection that died.
        handle: RelayHandle,
        session: RelaySessionKey,
        reason: DisconnectReason,
    },
    Frame {
        handle: RelayHandle,
        session: RelaySessionKey,
        frame: RelayFrame,
    },
    Health {
        /// The exact connection generation whose health changed. Like
        /// frames and disconnects, health delivery crosses the off-lock
        /// sink and may arrive after this slot has reopened.
        handle: RelayHandle,
        session: RelaySessionKey,
        health: RelayHealth,
    },
    /// A previously closed relay worker has actually exited and its OS
    /// thread has been joined. The engine uses this edge to retry exact
    /// required demand immediately, without polling a retiring budget.
    WorkerRetired,
    /// The one, ever, typed result for a durable `EVENT` handoff submitted
    /// via [`Pool::send_durable`] (issue #93). Delivered EXACTLY once per
    /// [`AttemptCorrelation`], unconditionally — never gated on the slot's
    /// current generation, never dropped because the slot has since closed
    /// or reconnected. Gating this like [`Self::Frame`] would risk silently
    /// stranding a correlation with no answer at all, which is exactly the
    /// hidden-queue failure mode this seam exists to remove.
    EventHandoff {
        correlation: AttemptCorrelation,
        result: HandoffResult,
    },
    /// The one, ever, typed terminal for an exact-generation ephemeral
    /// handoff started by [`Pool::send_ephemeral_exact`] (issue #883).
    /// Delivered EXACTLY once per [`EphemeralSendStart::Pending`] operation
    /// and, like [`Self::EventHandoff`], never gated on the slot's current
    /// generation: the worker decides the outcome once and transport only
    /// carries the value. Transport executes no consumer code, so a blocked
    /// or panicking reducer can never stall a relay worker's socket.
    ///
    /// `session` and `handle` are the EXACT target the operation was
    /// submitted against — not a re-lookup of whatever the slot holds now —
    /// so a consumer rejects a stale completion structurally instead of
    /// re-identifying it by relay URL, frame text, or filter hash.
    EphemeralHandoff {
        operation: EphemeralOperation,
        session: RelaySessionKey,
        handle: RelayHandle,
        outcome: EphemeralSendOutcome,
    },
}

/// Sink the pool pushes [`PoolEvent`]s onto. Implemented by
/// `nmp-engine`'s runtime edge, which translates each event into an
/// `EngineMsg` pushed onto the same inbox the engine thread reads from.
pub trait PoolEventSink: Send + Sync + 'static {
    fn on_event(&self, event: PoolEvent);
}

/// Blanket impl so a plain `std::sync::mpsc::Sender<PoolEvent>` satisfies
/// the sink bound directly — the common case for tests and small
/// standalone drivers. A disconnected receiver is swallowed (nothing left
/// to deliver to).
impl PoolEventSink for std::sync::mpsc::Sender<PoolEvent> {
    fn on_event(&self, event: PoolEvent) {
        let _ = self.send(event);
    }
}

impl PoolEventSink for std::sync::mpsc::SyncSender<PoolEvent> {
    fn on_event(&self, event: PoolEvent) {
        let _ = self.send(event);
    }
}

/// Construction-time knobs (bounded send/recv queues, reconnect backoff
/// bounds, keepalive interval — A2 fills in the concrete fields per the
/// harvested constants).
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Maximum distinct live relay workers. This is the transport half of
    /// the engine's one whole-demand relay ceiling; zero is normalized to
    /// [`DEFAULT_MAX_RELAYS`] and never disables admission.
    pub max_relays: usize,
    /// Maximum worker events waiting for the translator. A full queue blocks
    /// the socket worker, propagating pressure back to TCP reads.
    pub ingest_queue_capacity: usize,
    /// Maximum outbound commands (`Send`/`SendDurable`) in TRANSIT to one
    /// relay worker at once (issue #506's HIGH finding). `pool::worker::
    /// WorkerHandle::push` uses `try_send` against this bound, so a saturated
    /// channel surfaces as the EXISTING "not handed off" backpressure signal.
    /// Reconnect-preamble replacement and `Shutdown`/retire are exempt from
    /// this cap by construction (see those methods' docs), so a full data
    /// queue can neither retain stale reconnect ownership nor block teardown.
    ///
    /// This bounds transit, NOT memory, and for a while it was mistaken for
    /// both. A running worker drains this channel continuously into its own
    /// state, so each receive frees a slot and a producer can refill it
    /// forever. What a stalled or endlessly-redialing relay actually meets is
    /// `pool::worker`'s finite outbound envelope, which charges a frame from
    /// admission until a socket accepts it and so spans transit and worker
    /// retention together.
    pub command_queue_capacity: usize,
    /// Maximum translated pool events waiting for the engine bridge.
    pub event_sink_queue_capacity: usize,
    /// Maximum exact committed EVENT observations eligible for the preparse
    /// duplicate fast path. Eviction or zero capacity only causes ordinary
    /// parse/verify/store ingest.
    pub committed_observation_cache_capacity: usize,
    /// Maximum worker events drained into one ordered verification batch.
    pub max_verify_batch: usize,
    /// Maximum typed relay frames handed to the engine/store in one batch.
    /// This separately caps transaction size even if producers continuously
    /// refill the bounded event queue while the bridge is draining it.
    pub max_engine_batch: usize,
    /// Maximum conservative encoded bytes admitted to one engine/store batch.
    /// A single event larger than this bound is still admitted alone; the
    /// websocket message ceiling remains the absolute per-event bound.
    pub max_engine_batch_bytes: usize,
    /// Maximum time the engine bridge may wait for more consecutive EVENT
    /// frames after receiving the first one. Control frames and lifecycle
    /// events always end the batch immediately.
    pub max_engine_batch_wait: Duration,
    /// Override for the initial reconnect backoff delay; `None` uses the
    /// production default ([`crate::backoff::RECONNECT_DELAY_INITIAL`]).
    /// Integration tests that force a reconnect pass a small value so the
    /// test doesn't wait out the production 3s+jitter schedule.
    pub reconnect_delay_initial: Option<Duration>,
    /// Override for [`crate::backoff::jittered`]'s per-URL offset ceiling;
    /// `None` uses the production default
    /// ([`crate::backoff::RECONNECT_JITTER_MAX`]). The jitter is a FIXED
    /// value per URL, re-paid on every retry against that URL until it
    /// connects (see `jittered`'s doc) — for a same-process test relay that
    /// reconnects in milliseconds, an unlucky URL hash can otherwise tax
    /// every attempt up to ~5s apiece, dwarfing `reconnect_delay_initial`.
    /// Integration tests that force a reconnect pass `Some(Duration::ZERO)`
    /// so retries fire back-to-back instead of racing a per-URL lottery.
    pub reconnect_jitter_max: Option<Duration>,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_relays: DEFAULT_MAX_RELAYS,
            ingest_queue_capacity: 8_192,
            command_queue_capacity: 1_024,
            event_sink_queue_capacity: 8_192,
            committed_observation_cache_capacity: 131_072,
            max_verify_batch: 512,
            max_engine_batch: 4_096,
            max_engine_batch_bytes: 8 * 1024 * 1024,
            // A sub-millisecond wait lets socket/translator bursts form
            // bounded store transactions without imposing scheduler-scale
            // latency on an isolated EVENT. Representative ingest and exact
            // replay both improve at this value; larger waits regress again.
            max_engine_batch_wait: Duration::from_micros(200),
            reconnect_delay_initial: None,
            reconnect_jitter_max: None,
        }
    }
}

/// The generational WebSocket pool: `mio`-driven worker thread(s), one
/// socket per canonical relay URL (plan §3.2). Push-model only — there is
/// no `send_to_all`; the caller iterates its own routing plan.
///
/// Cheap to clone (`Arc` inside).
#[derive(Clone)]
pub struct Pool {
    inner: Arc<Mutex<PoolInner>>,
}

impl Pool {
    /// Construct a new pool. `sink` receives every [`PoolEvent`] until the
    /// pool is shut down (or the sink itself is dropped, for the blanket
    /// `mpsc::Sender` impl). The trust gate (`verifier`) is constructed by
    /// the engine and passed in; transport does not own crypto (#1677).
    pub fn new(
        cfg: PoolConfig,
        verifier: Verifier,
        sink: impl PoolEventSink,
    ) -> Result<Self, PoolBuildError> {
        Self::new_with_spawner(cfg, verifier, Arc::new(sink), system_spawner())
    }

    fn new_with_spawner(
        cfg: PoolConfig,
        verifier: Verifier,
        sink: Arc<dyn PoolEventSink>,
        spawner: Arc<dyn ThreadSpawner>,
    ) -> Result<Self, PoolBuildError> {
        Ok(Self {
            inner: PoolInner::try_new(cfg, verifier, sink, spawner)?,
        })
    }

    /// Ensure a worker is dialing/connected for `url`. Idempotent for a
    /// live slot (returns the current handle unchanged). If the URL was
    /// previously closed via [`Self::close`], the slot reopens with a fresh
    /// generation — the prior handle is now stale. Every refusal is returned
    /// as a typed error; this API never manufactures an invalid handle.
    pub fn ensure_open(&self, url: &RelayUrl) -> Result<RelayHandle, RelayOpenError> {
        self.ensure_session(&RelaySessionKey::public(url.clone()))
    }

    #[doc(hidden)]
    pub fn revalidate_committed_observations<'a>(
        &self,
        hits: impl IntoIterator<Item = &'a CommittedObservationHit>,
    ) -> bool {
        self.inner
            .lock()
            .is_ok_and(|inner| inner.committed_observations.revalidate_all(hits))
    }

    #[doc(hidden)]
    pub fn update_committed_observations(
        &self,
        invalidated: Vec<EventId>,
        published: Vec<CommittedObservationPublication>,
    ) {
        if let Ok(inner) = self.inner.lock() {
            inner
                .committed_observations
                .apply_update(invalidated, published);
        }
    }

    /// Ensure the exact physical relay session is dialing/connected.
    pub fn ensure_session(&self, session: &RelaySessionKey) -> Result<RelayHandle, RelayOpenError> {
        match self.inner.lock() {
            Ok(mut guard) => guard.try_ensure_session(session),
            Err(_) => Err(RelayOpenError::Unavailable),
        }
    }

    /// Return the current live generation for `url` without opening or
    /// reopening a worker. Used for best-effort close-only wire deltas: a
    /// withdrawn read relay must never be re-created merely to send `CLOSE`.
    pub fn live_handle(&self, url: &RelayUrl) -> Option<RelayHandle> {
        self.live_session_handle(&RelaySessionKey::public(url.clone()))
    }

    /// Return the current generation for one exact session without opening
    /// it.
    pub fn live_session_handle(&self, session: &RelaySessionKey) -> Option<RelayHandle> {
        match self.inner.lock() {
            Ok(guard) => guard.live_session_handle(session),
            Err(_) => None,
        }
    }

    /// Push one frame at one specific (URL, generation). A stale handle is
    /// a structural no-op (`false`) — the caller cannot accidentally target
    /// a superseded generation of the same URL.
    ///
    /// Returns `true` iff the worker accepted the frame into its finite
    /// outbound envelope — not iff it has been written to the socket, and
    /// never that a relay received it. The worker may still be dialing; the
    /// frame is retained until the socket opens.
    ///
    /// `false` is local backpressure and nothing else (issue #506): the
    /// worker is retaining as much ordinary outbound state as it may, the
    /// transit channel is full, the handle is stale, or the frame is larger
    /// than any relay could be handed. It never means a relay rejected
    /// anything — a rejection arrives as an inbound `OK`/`CLOSED`/`NOTICE`
    /// frame.
    ///
    /// Until #506's outbound-memory half this `true` was not a fact about
    /// memory at all: a running worker drains the transit channel
    /// continuously into its own uncapped state, so a caller facing a
    /// stalled relay got `true` forever while the process grew.
    pub fn send(&self, h: RelayHandle, frame: WireFrame) -> bool {
        let WireFrame::Text(text) = frame else {
            return false; // Binary is reserved; no wire-emittable path yet.
        };
        match self.inner.lock() {
            Ok(guard) => match guard.command_tx_for(h) {
                Some(worker) => worker.push_ordinary(text),
                None => false,
            },
            Err(_) => false,
        }
    }

    /// Hand off exactly one durable `EVENT` frame for one specific (URL,
    /// generation), correlated for exactly one async [`HandoffResult`]
    /// delivered via [`PoolEvent::EventHandoff`] (issue #93). Unlike
    /// [`Self::send`] (REQ/subscription traffic, fire-and-forget, may
    /// legitimately survive a reconnect via the preamble mechanism), a
    /// durable EVENT frame NEVER carries into a later connection
    /// generation: if the generation ends before the worker can confirm the
    /// write, the worker itself resolves and reports the correlation
    /// (`NotHandedOff` if still queued, `Ambiguous` if a write was accepted
    /// but never confirmed flushed) rather than silently requeuing it.
    ///
    /// [`DurableSendOutcome::Queued`] means the worker now owns the attempt
    /// and will later emit exactly one [`PoolEvent::EventHandoff`]. A stale
    /// handle, reserved binary frame, or disconnected command channel returns
    /// [`DurableSendOutcome::Resolved`] immediately, so the engine resolves
    /// it locally rather than sending back into its own bounded pool queue.
    pub fn send_durable(
        &self,
        h: RelayHandle,
        correlation: AttemptCorrelation,
        frame: WireFrame,
    ) -> DurableSendOutcome {
        let WireFrame::Text(text) = frame else {
            return DurableSendOutcome::Resolved(HandoffResult::NotHandedOff);
        };
        match self.inner.lock() {
            Ok(guard) => match guard.command_tx_for(h) {
                Some(worker) => {
                    let handed_off = worker.push(worker::WorkerCommand::SendDurable {
                        generation: h.generation,
                        correlation,
                        frame: text,
                    });
                    if handed_off {
                        DurableSendOutcome::Queued
                    } else {
                        DurableSendOutcome::Resolved(HandoffResult::NotHandedOff)
                    }
                }
                None => DurableSendOutcome::Resolved(HandoffResult::NotHandedOff),
            },
            Err(_) => DurableSendOutcome::Resolved(HandoffResult::NotHandedOff),
        }
    }

    /// Hand off one connection-scoped frame only to the exact currently
    /// connected `(session, handle)`.
    ///
    /// Unlike [`Self::send`], this operation is rejected while dialing or
    /// disconnected and is never placed in the ordinary reconnecting queue
    /// or reconnect preamble. Unlike [`Self::send_durable`], it has no
    /// [`AttemptCorrelation`] and never enters durable EVENT bookkeeping.
    /// The worker rechecks `handle.generation` when it drains the command;
    /// a reconnect racing this call therefore resolves `Unavailable` rather
    /// than carrying the frame into the new socket.
    ///
    /// An [`EphemeralSendStart::Pending`] operation resolves exactly once,
    /// through one [`PoolEvent::EphemeralHandoff`] on the ordinary pool event
    /// path, after either a successful write+flush on that generation
    /// (`Accepted`) or a stale generation / connection end (`Unavailable`).
    /// The pool never runs caller code: `operation` is an opaque token the
    /// caller reduces on its own owner thread. Synchronous refusal —
    /// including a full bounded command queue, the same backpressure signal
    /// [`Self::send_durable`] reports as `NotHandedOff` — returns
    /// [`EphemeralSendStart::Resolved`] and emits no event at all.
    pub fn send_ephemeral_exact(
        &self,
        session: &RelaySessionKey,
        h: RelayHandle,
        operation: EphemeralOperation,
        frame: WireFrame,
    ) -> EphemeralSendStart {
        let WireFrame::Text(text) = frame else {
            return EphemeralSendStart::Resolved(EphemeralSendOutcome::Unavailable);
        };
        let Ok(guard) = self.inner.lock() else {
            return EphemeralSendStart::Resolved(EphemeralSendOutcome::Unavailable);
        };
        let Some(worker) = guard.connected_command_tx_for(session, h) else {
            return EphemeralSendStart::Resolved(EphemeralSendOutcome::Unavailable);
        };
        let handed_off = worker.push(worker::WorkerCommand::SendEphemeral {
            target: worker::EphemeralTarget {
                session: session.clone(),
                generation: h.generation,
                operation,
            },
            frame: text,
        });
        if handed_off {
            EphemeralSendStart::Pending
        } else {
            EphemeralSendStart::Resolved(EphemeralSendOutcome::Unavailable)
        }
    }

    /// Close the slot for `h` and return its synchronous disconnect fact.
    /// A stale/already-closed handle returns `None`. The fact is returned,
    /// never delivered through the blocking pool sink while `PoolInner` is
    /// locked. A subsequent [`Self::ensure_open`] reopens a fresh generation.
    pub fn close(&self, h: RelayHandle) -> Option<PoolEvent> {
        match self.inner.lock() {
            Ok(mut guard) => guard.close(h),
            Err(_) => None,
        }
    }

    /// Release every live physical session absent from the exact caller-owned
    /// session set. This is the release half of the finite admission
    /// contract: a caller that owns the exact current relay-demand set can
    /// free obsolete slots before opening replacement relays, while retaining
    /// every read or write lane that is still live.
    ///
    /// The pool does not infer demand from traffic. The engine supplies the
    /// authoritative union of its current read plan and nonterminal write
    /// lanes, so transport cannot accidentally evict an in-flight write or
    /// keep historical read workers forever.
    ///
    /// Caller-supplied final connection-scoped text frames are flushed on
    /// their exact current generations before retirement; transport never
    /// interprets the text.
    pub fn close_unrequired_sessions(
        &self,
        required: &BTreeSet<RelaySessionKey>,
        frames: BTreeMap<RelaySessionKey, Vec<String>>,
    ) -> Vec<PoolEvent> {
        match self.inner.lock() {
            Ok(mut guard) => guard.close_unrequired_sessions(required, frames),
            Err(_) => Vec::new(),
        }
    }

    /// Register a reconnect preamble for the worker at handle `h`.
    ///
    /// On every subsequent (re)connect the worker injects these frames at
    /// the FRONT of its outbound queue before draining any newly-posted
    /// `send`. This is the structural REQ-before-EVENT guarantee: a
    /// subscription REQ registered here is always on the wire before any
    /// EVENT the caller enqueues after observing `PoolEvent::Connected`.
    ///
    /// The preamble survives every reconnect (not cleared after use); the
    /// last call wins, including while the current worker is disconnected or
    /// dialing. Its finite replacement is independent of the bounded ordinary
    /// command lane. Returns `true` iff the current worker recorded the
    /// replacement; a stale or closed handle returns `false`.
    pub fn set_reconnect_preamble(&self, h: RelayHandle, frames: Vec<String>) -> bool {
        self.begin_reconnect_preamble(h, frames).is_some()
    }

    /// Begin a reconnect-preamble ownership transition without holding the
    /// pool mutex while a previously accepted socket write settles.
    ///
    /// The returned finite transition is immediately `Some(true)` when no
    /// old replay is unflushed, remains pending while one is, and resolves
    /// after that old write flushes or its generation ends. A stale or closed
    /// handle returns `None`.
    pub fn begin_reconnect_preamble(
        &self,
        h: RelayHandle,
        frames: Vec<String>,
    ) -> Option<ReconnectPreambleTransition> {
        let registration = self
            .inner
            .lock()
            .ok()?
            .reconnect_preamble_registration_for(h)?;
        Some(registration.replace(frames))
    }

    /// Schedule the registered reconnect preamble on this exact connected
    /// generation.
    ///
    /// Unlike [`Pool::send`], the scheduled frames remain revision-aware
    /// until their individual socket writes. Replacing the registered
    /// preamble before a write starts revokes the stale pending replay. The
    /// finite replay request is independent of the bounded ordinary command
    /// lane; a stale or disconnected handle returns `false`.
    pub fn replay_reconnect_preamble(&self, h: RelayHandle) -> bool {
        match self.inner.lock() {
            Ok(guard) => guard.replay_reconnect_preamble_for(h),
            Err(_) => false,
        }
    }

    /// Open this exact connected generation's ordinary outbound gate after
    /// the consumer has applied [`PoolEvent::InitialReadCompleted`] (or has
    /// completed authentication ordered ahead of it). A stale generation is
    /// a structural no-op.
    pub fn release_initial_read(&self, h: RelayHandle) -> bool {
        match self.inner.lock() {
            Ok(guard) => guard.release_initial_read_for(h),
            Err(_) => false,
        }
    }

    /// Per-handle health snapshot. A stale handle returns `None`.
    #[must_use]
    pub fn health(&self, h: RelayHandle) -> Option<RelayHealth> {
        self.inner.lock().ok().and_then(|g| g.health_for(h))
    }

    /// Monotonic count of [`Self::ensure_open`] calls this pool refused
    /// because opening the relay would have exceeded [`PoolConfig::max_relays`]
    /// live workers. The engine folds this into its diagnostics rejection
    /// counter — see `nmp-engine`'s relay admission. A poisoned lock reports
    /// `0` (nothing to report through a broken pool), matching every other
    /// read on this facade.
    #[must_use]
    pub fn admission_rejections(&self) -> u64 {
        self.inner
            .lock()
            .map(|g| g.relays_rejected_over_cap())
            .unwrap_or(0)
    }

    /// Tear down every worker. Subsequent [`Self::ensure_open`] calls return
    /// [`RelayOpenError::ShuttingDown`]; subsequent `send` calls are
    /// structural no-ops. Joins the translator thread before returning.
    pub fn shutdown(&self) {
        let handles = match self.inner.lock() {
            Ok(mut guard) => guard.shutdown(),
            Err(_) => return,
        };
        handles.join();
    }
}

#[cfg(test)]
mod thread_budget_tests {
    use super::spawn::test_support::RefusingThreadSpawner;
    use super::verify::DEFAULT_VERIFIER_WORKERS;
    use super::*;
    use std::sync::{mpsc, Arc};

    fn test_pool(
        successful_spawns: usize,
        max_relays: usize,
    ) -> (
        Arc<RefusingThreadSpawner>,
        Result<Pool, PoolBuildError>,
        mpsc::Receiver<PoolEvent>,
    ) {
        let spawner = Arc::new(RefusingThreadSpawner::after(successful_spawns));
        let erased: Arc<dyn ThreadSpawner> = spawner.clone();
        let (sink, events) = mpsc::channel();
        // The verifier is constructed BEFORE the pool (#1677: the engine
        // owns the trust gate and hands it to `Pool::new`), so its workers
        // spawn first against the same injected spawner. A construction
        // refusal here is surfaced as a typed `PoolBuildError`.
        let verifier = match Verifier::new_with_spawner(
            VerifyConfig {
                workers: DEFAULT_VERIFIER_WORKERS,
                queue_capacity: 64,
                lru_capacity: 131_072,
            },
            Arc::new(NullKnownSig),
            erased.clone(),
        ) {
            Ok(verifier) => verifier,
            Err(error) => {
                return (
                    spawner,
                    Err(PoolBuildError::ThreadUnavailable(error)),
                    events,
                )
            }
        };
        let pool = Pool::new_with_spawner(
            PoolConfig {
                max_relays,
                ..PoolConfig::default()
            },
            verifier,
            Arc::new(sink),
            erased,
        );
        (spawner, pool, events)
    }

    #[test]
    fn injected_construction_refusals_are_typed_and_cleanup_exactly() {
        // Spawn order under #1677: the verifier is constructed before the
        // pool, so its two workers spawn first, then the reaper, then the
        // translator. A refusal at any step is typed and cleans up exactly.
        for (allowed, expected_role) in [
            (0, ThreadRole::VerifierWorker),
            (1, ThreadRole::VerifierWorker),
            (2, ThreadRole::RetirementReaper),
            (3, ThreadRole::PoolTranslator),
        ] {
            let (spawner, result, _events) = test_pool(allowed, 1);
            let error = match result {
                Err(PoolBuildError::ThreadUnavailable(error)) => error,
                _ => panic!("injected spawn refusal must stay typed"),
            };
            assert_eq!(error.role, expected_role);
            assert_eq!(error.reason, "injected thread pressure");
            assert_eq!(
                spawner.live(),
                0,
                "partial construction must join all threads"
            );
        }
    }

    #[test]
    fn relay_spawn_refusal_is_typed_without_publishing_a_slot() {
        // two verifier workers + reaper + translator succeed; relay fails.
        let (spawner, pool, _events) = test_pool(4, 1);
        let pool = pool.expect("fixed engine executors fit the injected budget");
        let relay = RelayUrl::parse("ws://127.0.0.1:9").unwrap();
        let error = pool.ensure_open(&relay).unwrap_err();
        assert!(matches!(
            error,
            RelayOpenError::ThreadUnavailable(ThreadSpawnError {
                role: ThreadRole::RelayWorker,
                ..
            })
        ));
        assert!(pool.live_handle(&relay).is_none());
        assert_eq!(spawner.live(), 4);
        pool.shutdown();
        assert_eq!(spawner.live(), 0);
    }

    #[test]
    fn cap_sized_churn_never_exceeds_active_plus_retiring_envelope_and_joins() {
        let (spawner, pool, _events) = test_pool(usize::MAX, 1);
        let pool = pool.unwrap();
        let first = RelayUrl::parse("ws://127.0.0.1:9").unwrap();
        let second = RelayUrl::parse("ws://127.0.0.1:10").unwrap();
        let first_handle = pool.ensure_open(&first).unwrap();
        pool.close(first_handle).unwrap();
        let second_handle = pool.ensure_open(&second).unwrap();
        pool.close(second_handle).unwrap();

        // Four fixed engine executors + at most two relay OS threads: one
        // active allowance and one retirement allowance.
        assert!(spawner.peak() <= 6, "relay churn escaped the 2x envelope");
        pool.shutdown();
        assert_eq!(spawner.live(), 0, "shutdown is an exact join barrier");
    }
}

#[cfg(test)]
mod ephemeral_send_tests {
    use super::*;
    use nmp_grammar::AccessContext;
    use nostr::Keys;
    use std::sync::{mpsc, Arc};

    #[test]
    fn dialing_stale_wrong_session_and_binary_handoffs_fail_synchronously() {
        let (events, event_rx) = mpsc::channel();
        let verifier = Verifier::new(VerifyConfig::default(), Arc::new(NullKnownSig)).unwrap();
        let pool = Pool::new(
            PoolConfig {
                reconnect_delay_initial: Some(Duration::from_secs(30)),
                reconnect_jitter_max: Some(Duration::ZERO),
                ..PoolConfig::default()
            },
            verifier,
            events,
        )
        .unwrap();
        let relay = RelayUrl::parse("ws://127.0.0.1:9").unwrap();
        let session = RelaySessionKey::new(
            relay.clone(),
            AccessContext::Nip42(Keys::generate().public_key()),
        );
        let handle = pool.ensure_session(&session).unwrap();
        let wrong_session =
            RelaySessionKey::new(relay, AccessContext::Nip42(Keys::generate().public_key()));
        let stale = RelayHandle {
            generation: handle.generation.wrapping_add(1),
            ..handle
        };

        for (index, (candidate_session, candidate_handle, frame)) in [
            (&session, handle, WireFrame::Text("dialing".to_string())),
            (&session, stale, WireFrame::Text("stale".to_string())),
            (
                &wrong_session,
                handle,
                WireFrame::Text("wrong-session".to_string()),
            ),
            (&session, handle, WireFrame::Binary(vec![1, 2, 3])),
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(
                pool.send_ephemeral_exact(
                    candidate_session,
                    candidate_handle,
                    EphemeralOperation(index as u64),
                    frame,
                ),
                EphemeralSendStart::Resolved(EphemeralSendOutcome::Unavailable)
            );
        }
        assert!(
            !event_rx
                .try_iter()
                .any(|event| matches!(event, PoolEvent::EphemeralHandoff { .. })),
            "a synchronous refusal must never also emit a terminal completion"
        );

        pool.shutdown();
    }
}
