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

use std::sync::{Arc, Mutex};
use std::time::Duration;

use nostr::{Event, RelayMessage, RelayUrl, SubscriptionId};

use crate::handle::RelayHandle;
use crate::health::RelayHealth;

mod connect;
mod frame;
mod inner;
mod verify;
mod worker;

use inner::PoolInner;

/// Safe default for the single engine/transport relay ceiling. Zero is
/// normalized to this value as well, so legacy/default construction cannot
/// silently re-enable unbounded worker growth.
pub const DEFAULT_MAX_RELAYS: usize = 10;

/// A typed refusal to create or recover a relay worker.
///
/// Callers must handle this result before they receive a [`RelayHandle`], so
/// a relay-cap refusal cannot be mistaken for a live generation and silently
/// fed into [`Pool::send`] as an opaque sentinel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayOpenError {
    /// Opening another live worker would exceed the pool-wide ceiling.
    AtCapacity { max_relays: usize },
    /// The pool has entered terminal shutdown and cannot reopen workers.
    ShuttingDown,
    /// Pool state was poisoned; fail closed instead of returning a handle.
    Unavailable,
}

impl std::fmt::Display for RelayOpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AtCapacity { max_relays } => {
                write!(f, "relay pool capacity {max_relays} exhausted")
            }
            Self::ShuttingDown => f.write_str("relay pool is shutting down"),
            Self::Unavailable => f.write_str("relay pool state is unavailable"),
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
    /// A transient or permanent transport error (dial failure, socket
    /// error, peer-initiated close, keepalive timeout) tore down a
    /// previously-`Connected` session. [`Pool::health`] carries the message
    /// and, for a transient error, the next retry delay.
    Error,
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
        slot: u32,
        reason: DisconnectReason,
    },
    Frame {
        handle: RelayHandle,
        frame: RelayFrame,
    },
    Health {
        slot: u32,
        health: RelayHealth,
    },
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
    pub max_relays: usize,
    /// Maximum worker events waiting for the translator. A full queue blocks
    /// the socket worker, propagating pressure back to TCP reads.
    pub ingest_queue_capacity: usize,
    /// Maximum translated pool events waiting for the engine bridge.
    pub event_sink_queue_capacity: usize,
    /// Persistent native verification workers. Zero selects available host
    /// parallelism; wasm always uses the deterministic sequential path.
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
    #[must_use]
    pub fn new(cfg: PoolConfig, sink: impl PoolEventSink) -> Self {
        Self {
            inner: PoolInner::new(cfg, Arc::new(sink)),
        }
    }

    /// Ensure a worker is dialing/connected for `url`. Idempotent for a
    /// live slot (returns the current handle unchanged). If the URL was
    /// previously closed via [`Self::close`], the slot reopens with a fresh
    /// generation — the prior handle is now stale. Every refusal is returned
    /// as a typed error; this API never manufactures an invalid handle.
    pub fn ensure_open(&self, url: &RelayUrl) -> Result<RelayHandle, RelayOpenError> {
        match self.inner.lock() {
            Ok(mut guard) => {
                let handle = guard.ensure_open(url);
                if handle.slot == u32::MAX {
                    Err(guard.open_refusal())
                } else {
                    Ok(handle)
                }
            }
            Err(_) => Err(RelayOpenError::Unavailable),
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
    /// live workers (issue #121). Always `0` unless an operator configured a
    /// non-zero cap. The engine folds this into its diagnostics rejection
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

    /// Tear down every worker. Subsequent [`Self::ensure_open`] calls
    /// return a sentinel dead handle; subsequent `send` calls are
    /// structural no-ops. Joins the translator thread before returning.
    pub fn shutdown(&self) {
        let handle = match self.inner.lock() {
            Ok(mut guard) => guard.shutdown(),
            Err(_) => None,
        };
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }
}
