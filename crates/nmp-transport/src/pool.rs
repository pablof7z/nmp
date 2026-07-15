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

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nostr::{Event, RelayMessage, RelayUrl, SubscriptionId};

use crate::handle::RelayHandle;
use crate::health::RelayHealth;

mod connect;
mod frame;
mod inner;
mod spawn;
mod verify;
mod worker;

use inner::PoolInner;

/// Safe default for the single engine/transport relay ceiling. Zero is
/// normalized to this value as well, so legacy/default construction cannot
/// silently re-enable unbounded worker growth.
pub const DEFAULT_MAX_RELAYS: usize = 10;

/// Small fixed verifier set owned by one engine. Signature verification is
/// CPU-bound and fed through bounded queues; copying host parallelism into
/// every engine multiplied OS threads without imposing a process budget.
pub const DEFAULT_VERIFIER_WORKERS: usize = 2;

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

/// One parsed, owned relay message off the wire.
///
/// Text is parsed exactly once at the transport boundary. EVENT payloads move
/// immediately into an [`Arc`], so signature workers and the engine share the
/// same parsed allocation instead of deep-cloning content and tags. Keepalive
/// `Ping`/`Pong`, binary messages, and the
/// WebSocket `Close` frame never reach this type — they are consumed by the
/// worker's keepalive FSM / surfaced instead as [`PoolEvent::Disconnected`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayFrame {
    Event {
        subscription_id: SubscriptionId,
        event: Arc<Event>,
    },
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
            },
            message => Self::Message(Box::new(message)),
        }
    }

    /// Borrow an EVENT payload through its shared parsed allocation.
    #[must_use]
    pub fn event(&self) -> Option<&Arc<Event>> {
        match self {
            Self::Event { event, .. } => Some(event),
            Self::Message(_) => None,
        }
    }

    /// Move an EVENT into the engine, normally without cloning.
    ///
    /// The translator drops every temporary verifier reference before sink
    /// delivery, making `Arc::try_unwrap` the production path. The clone is a
    /// defensive fallback for public callers that retained a frame clone.
    pub fn into_event(self) -> Result<Event, Self> {
        match self {
            Self::Event { event, .. } => {
                Ok(Arc::try_unwrap(event).unwrap_or_else(|event| event.as_ref().clone()))
            }
            other => Err(other),
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
            } => RelayMessage::event(
                subscription_id,
                Arc::try_unwrap(event).unwrap_or_else(|event| event.as_ref().clone()),
            ),
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
    /// `Pool::shutdown` tore down every worker in the pool.
    ShuttingDown,
}

/// Events the pool pushes to its [`PoolEventSink`]. Reconnect always mints
/// a NEW generation for the slot (ledger #2/#3/#4) — `Connected` carries
/// the fresh [`RelayHandle`].
#[derive(Debug, Clone)]
pub enum PoolEvent {
    Connected {
        handle: RelayHandle,
        url: RelayUrl,
    },
    Disconnected {
        /// The exact connection generation that disconnected. A slot may
        /// already have reopened by the time this event is reduced, so a
        /// bare slot number cannot safely identify the connection that died.
        handle: RelayHandle,
        reason: DisconnectReason,
    },
    Frame {
        handle: RelayHandle,
        frame: RelayFrame,
    },
    Health {
        /// The exact connection generation whose health changed. Like
        /// frames and disconnects, health delivery crosses the off-lock
        /// sink and may arrive after this slot has reopened.
        handle: RelayHandle,
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
    /// Maximum outbound commands (`Send`/`SendDurable`/reconnect-preamble
    /// updates) queued per relay worker (issue #506's HIGH finding). This is
    /// the one pool queue that was historically unbounded: a stalled-but-
    /// connected socket (TCP send window full, so `flush_writes` keeps
    /// returning `Blocked`) could accumulate an unbounded backlog while
    /// `Pool::send`/`send_durable` kept reporting success. `pool::worker::
    /// WorkerHandle::push` now uses `try_send` against this bound, so a
    /// saturated queue surfaces as the EXISTING "not handed off" backpressure
    /// signal instead of unbounded memory growth. `Shutdown`/retire is exempt
    /// from this cap by construction (see that type's `retire` doc), so a
    /// full data queue can never block a worker from being torn down.
    pub command_queue_capacity: usize,
    /// Maximum translated pool events waiting for the engine bridge.
    pub event_sink_queue_capacity: usize,
    /// Persistent native verification workers. Zero selects the small fixed
    /// [`DEFAULT_VERIFIER_WORKERS`] set; it never mirrors host parallelism.
    pub verifier_workers: usize,
    /// Maximum verification tasks queued at each persistent worker.
    pub verifier_queue_capacity: usize,
    /// Maximum verified id/signature entries retained by the translator.
    /// Eviction only causes later re-verification; it never changes policy.
    pub verified_cache_capacity: usize,
    /// Maximum worker events drained into one ordered verification batch.
    pub max_verify_batch: usize,
    /// Maximum typed relay frames handed to the engine/store in one batch.
    /// This separately caps transaction size even if producers continuously
    /// refill the bounded event queue while the bridge is draining it.
    pub max_engine_batch: usize,
    /// Override for the keepalive idle threshold; `None` uses the
    /// production default ([`crate::keepalive::KEEPALIVE_IDLE_THRESHOLD`]).
    /// Tests on millisecond budgets pass a small value.
    pub keepalive_idle: Option<Duration>,
    /// Override for the keepalive pong timeout; `None` uses the production
    /// default ([`crate::keepalive::KEEPALIVE_PONG_TIMEOUT`]).
    pub keepalive_pong_timeout: Option<Duration>,
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
            ingest_queue_capacity: 1_024,
            command_queue_capacity: 1_024,
            event_sink_queue_capacity: 1_024,
            verifier_workers: 0,
            verifier_queue_capacity: 64,
            verified_cache_capacity: 65_536,
            max_verify_batch: 128,
            max_engine_batch: 128,
            keepalive_idle: None,
            keepalive_pong_timeout: None,
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
    /// `mpsc::Sender` impl).
    pub fn new(cfg: PoolConfig, sink: impl PoolEventSink) -> Result<Self, PoolBuildError> {
        Self::new_with_spawner(cfg, Arc::new(sink), spawn::system_spawner())
    }

    fn new_with_spawner(
        cfg: PoolConfig,
        sink: Arc<dyn PoolEventSink>,
        spawner: Arc<dyn spawn::ThreadSpawner>,
    ) -> Result<Self, PoolBuildError> {
        Ok(Self {
            inner: PoolInner::try_new(cfg, sink, spawner)?,
        })
    }

    /// Ensure a worker is dialing/connected for `url`. Idempotent for a
    /// live slot (returns the current handle unchanged). If the URL was
    /// previously closed via [`Self::close`], the slot reopens with a fresh
    /// generation — the prior handle is now stale. Every refusal is returned
    /// as a typed error; this API never manufactures an invalid handle.
    pub fn ensure_open(&self, url: &RelayUrl) -> Result<RelayHandle, RelayOpenError> {
        match self.inner.lock() {
            Ok(mut guard) => guard.try_ensure_open(url),
            Err(_) => Err(RelayOpenError::Unavailable),
        }
    }

    /// Return the current live generation for `url` without opening or
    /// reopening a worker. Used for best-effort close-only wire deltas: a
    /// withdrawn read relay must never be re-created merely to send `CLOSE`.
    pub fn live_handle(&self, url: &RelayUrl) -> Option<RelayHandle> {
        match self.inner.lock() {
            Ok(guard) => guard.live_handle(url),
            Err(_) => None,
        }
    }

    /// Push one frame at one specific (URL, generation). A stale handle is
    /// a structural no-op (`false`) — the caller cannot accidentally target
    /// a superseded generation of the same URL.
    ///
    /// Returns `true` iff the frame was handed to the worker's outbound
    /// queue — not iff it has been written to the socket. The worker may
    /// still be dialing; the frame is queued until the socket opens.
    pub fn send(&self, h: RelayHandle, frame: WireFrame) -> bool {
        let WireFrame::Text(text) = frame else {
            return false; // Binary is reserved; no wire-emittable path yet.
        };
        match self.inner.lock() {
            Ok(guard) => match guard.command_tx_for(h) {
                Some(worker) => worker.push(worker::WorkerCommand::Send(text)),
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

    /// Close every live worker whose URL is absent from `required` and
    /// return each synchronous disconnect fact. This is the release half of
    /// the finite admission contract: a caller that owns the exact current
    /// relay-demand set can free obsolete slots before opening replacement
    /// relays, while retaining every read or write lane that is still live.
    ///
    /// The pool does not infer demand from traffic. The engine supplies the
    /// authoritative union of its current read plan and nonterminal write
    /// lanes, so transport cannot accidentally evict an in-flight write or
    /// keep historical read workers forever.
    pub fn close_unrequired(&self, required: &BTreeSet<RelayUrl>) -> Vec<PoolEvent> {
        match self.inner.lock() {
            Ok(mut guard) => guard.close_unrequired(required),
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
    /// last call wins. Returns `true` iff enqueued; a stale or closed
    /// handle returns `false`.
    pub fn set_reconnect_preamble(&self, h: RelayHandle, frames: Vec<String>) -> bool {
        match self.inner.lock() {
            Ok(guard) => guard.set_reconnect_preamble_for(h, frames),
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
        let erased: Arc<dyn spawn::ThreadSpawner> = spawner.clone();
        let (sink, events) = mpsc::channel();
        let pool = Pool::new_with_spawner(
            PoolConfig {
                max_relays,
                ..PoolConfig::default()
            },
            Arc::new(sink),
            erased,
        );
        (spawner, pool, events)
    }

    #[test]
    fn injected_construction_refusals_are_typed_and_cleanup_exactly() {
        for (allowed, expected_role) in [
            (0, ThreadRole::RetirementReaper),
            (1, ThreadRole::VerifierWorker),
            (2, ThreadRole::VerifierWorker),
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
        // reaper + two verifier workers + translator succeed; relay fails.
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

    #[test]
    fn verifier_worker_configuration_saturates_at_fixed_engine_budget() {
        assert_eq!(
            inner::configured_verifier_workers(0),
            DEFAULT_VERIFIER_WORKERS
        );
        assert_eq!(inner::configured_verifier_workers(1), 1);
        assert_eq!(
            inner::configured_verifier_workers(usize::MAX),
            DEFAULT_VERIFIER_WORKERS
        );
    }
}
