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
    /// The returned `bool` mirrors [`Self::send`]'s (`true` iff handed to a
    /// live worker's outbound queue) but is NOT the authoritative answer —
    /// the caller must watch for `PoolEvent::EventHandoff{correlation, ..}`
    /// either way. A stale handle or a worker whose command channel has
    /// already disconnected resolves the correlation synchronously, right
    /// here, as `NotHandedOff` (provably true: it never reached any live
    /// worker at all) — the caller never needs a separate "did `false` mean
    /// I should treat this as resolved" branch.
    pub fn send_durable(
        &self,
        h: RelayHandle,
        correlation: AttemptCorrelation,
        frame: WireFrame,
    ) -> bool {
        let WireFrame::Text(text) = frame else {
            self.resolve_not_handed_off(correlation);
            return false; // Binary is reserved; no wire-emittable path yet.
        };
        let outcome = match self.inner.lock() {
            Ok(guard) => match guard.command_tx_for(h) {
                Some(worker) => {
                    let handed_off = worker.push(worker::WorkerCommand::SendDurable {
                        generation: h.generation,
                        correlation,
                        frame: text,
                    });
                    if handed_off {
                        None
                    } else {
                        Some(guard.sink())
                    }
                }
                None => Some(guard.sink()),
            },
            Err(poisoned) => Some(poisoned.into_inner().sink()),
        };
        match outcome {
            Some(sink) => {
                sink.on_event(PoolEvent::EventHandoff {
                    correlation,
                    result: HandoffResult::NotHandedOff,
                });
                false
            }
            None => true,
        }
    }

    /// Synchronously resolve `correlation` as `NotHandedOff` when the frame
    /// could not even be considered for handoff (e.g. a non-text `WireFrame`
    /// — reserved, never wire-emittable today). Mirrors the stale-handle
    /// path in [`Self::send_durable`] so every call resolves exactly once
    /// regardless of which early-return fires.
    fn resolve_not_handed_off(&self, correlation: AttemptCorrelation) {
        let sink = match self.inner.lock() {
            Ok(guard) => guard.sink(),
            Err(poisoned) => poisoned.into_inner().sink(),
        };
        sink.on_event(PoolEvent::EventHandoff {
            correlation,
            result: HandoffResult::NotHandedOff,
        });
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

/// Sentinel handle returned when the pool's lock is poisoned or the pool
/// has already been shut down — matches every other stale-handle path
/// (`send`/`close`/`health` on it are all structural no-ops).
fn dead_handle() -> RelayHandle {
    RelayHandle {
        slot: u32::MAX,
        generation: 0,
    }
}
