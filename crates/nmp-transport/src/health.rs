//! Per-relay connection health (M3 plan §3.2), driving the reconnect/
//! backoff/keepalive FSM A2 harvests from the old repo's
//! `relay_worker`/`keepalive.rs`.

use std::time::Duration;

/// = the old repo's connection-lifecycle FSM states, re-cut for the new
/// reducer vocabulary. A2 decides the exact transition set.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
pub enum ConnState {
    #[default]
    Connecting,
    Connected,
    Disconnected,
}

/// Observable health snapshot for one relay slot.
///
/// Not `Copy` (carries an owned `last_error` message) — call sites clone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RelayHealth {
    pub state: ConnState,
    pub backoff: Duration,
    pub last_rtt: Option<Duration>,
    /// Human-readable message from the most recent connect/read/write
    /// failure. Cleared on a fresh `Connected`.
    pub last_error: Option<String>,
}
