//! Per-relay connection health (M3 plan §3.2), driving the reconnect/
//! backoff/keepalive FSM A2 harvests from the old repo's
//! `relay_worker`/`keepalive.rs`.

use std::time::Duration;

/// = the old repo's connection-lifecycle FSM states, re-cut for the new
/// reducer vocabulary. A2 decides the exact transition set.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ConnState {
    Connecting,
    Connected,
    Disconnected,
}

/// Observable health snapshot for one relay slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelayHealth {
    pub state: ConnState,
    pub backoff: Duration,
    pub last_rtt: Option<Duration>,
}
