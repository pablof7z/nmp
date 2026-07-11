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

use nostr::RelayUrl;

use crate::handle::RelayHandle;
use crate::health::RelayHealth;

mod connect;
mod frame;
mod inner;
mod verify;
mod worker;

use inner::PoolInner;

/// A frame handed to the pool for sending. Substrate-grade: no "kind"/
/// "pubkey" here — the pool moves bytes, it never interprets Nostr
/// semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireFrame {
    Text(String),
    Binary(Vec<u8>),
}

/// An inbound frame off the wire. The pool pre-classifies `AUTH` only;
/// everything else stays opaque text for the engine reducer to parse
/// (`EVENT`/`EOSE`/`OK`/`CLOSED`/`NEG-*`). Keepalive `Ping`/`Pong` and the
/// WebSocket `Close` frame never reach this type — they are consumed by the
/// worker's keepalive FSM / surfaced instead as [`PoolEvent::Disconnected`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayFrame {
    Text(String),
    Auth(String),
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

/// Construction-time knobs (bounded send/recv queues, reconnect backoff
/// bounds, keepalive interval — A2 fills in the concrete fields per the
/// harvested constants).
#[derive(Debug, Clone, Default)]
pub struct PoolConfig {
    pub max_relays: usize,
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
    /// generation — the prior handle is now stale.
    pub fn ensure_open(&self, url: &RelayUrl) -> RelayHandle {
        match self.inner.lock() {
            Ok(mut guard) => guard.ensure_open(url),
            Err(_) => dead_handle(),
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

    /// Close the slot for `h`. No-op if the handle is stale or the slot was
    /// already closed. A subsequent [`Self::ensure_open`] for the same URL
    /// reopens with a bumped generation.
    pub fn close(&self, h: RelayHandle) -> bool {
        match self.inner.lock() {
            Ok(mut guard) => guard.close(h),
            Err(_) => false,
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

/// Sentinel handle returned when the pool's lock is poisoned or the pool
/// has already been shut down — matches every other stale-handle path
/// (`send`/`close`/`health` on it are all structural no-ops).
fn dead_handle() -> RelayHandle {
    RelayHandle {
        slot: u32::MAX,
        generation: 0,
    }
}
