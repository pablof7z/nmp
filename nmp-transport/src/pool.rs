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
//! Step 0 declares the wire/event vocabulary only. A2 fills in `Pool`'s
//! `new`/`ensure_open`/`send`/`close`/`set_reconnect_preamble`/`health`/
//! `shutdown` (the exact signatures are in plan §3.2's `impl Pool` sketch).

use nostr::RelayUrl;

use crate::handle::RelayHandle;
use crate::health::RelayHealth;

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
/// (`EVENT`/`EOSE`/`OK`/`CLOSED`/`NEG-*`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayFrame {
    Text(String),
    Auth(String),
}

/// Why a relay slot disconnected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisconnectReason {
    Closed,
    Error,
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
pub trait PoolEventSink: Send {
    fn on_event(&self, event: PoolEvent);
}

/// Construction-time knobs (bounded send/recv queues, reconnect backoff
/// bounds, keepalive interval — A2 fills in the concrete fields per the
/// harvested constants).
#[derive(Debug, Clone, Default)]
pub struct PoolConfig {
    pub max_relays: usize,
}

/// The generational WebSocket pool: `mio`-driven worker thread(s), one
/// socket per canonical relay URL (plan §3.2). Push-model only — there is
/// no `send_to_all`; the caller iterates its own routing plan.
///
/// Step 0: empty shell. A2 owns the internal fields (mio `Poll`, per-slot
/// connection state, reconnect/backoff timers) and the `impl Pool` block:
/// `new(cfg, sink) -> Self`, `ensure_open(&url) -> RelayHandle`,
/// `send(handle, frame) -> bool` (false if `handle` is stale),
/// `close(handle) -> bool`, `set_reconnect_preamble(handle, frames)`,
/// `health(handle) -> Option<RelayHealth>`, `shutdown()`.
pub struct Pool;
