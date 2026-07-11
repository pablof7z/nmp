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
    /// Count of `EVENT` frames from this relay that FAILED the ingest
    /// signature-verification gate (`pool::verify::gate`) -- a schnorr
    /// verification failure on first sight, or a signature that mismatched
    /// a previously-verified value for the same event id. This is a relay
    /// MISBEHAVIOR signal, not a routine drop: a well-behaved relay never
    /// produces a nonzero count here. Never cleared by a reconnect (unlike
    /// `last_error`) -- it is a lifetime tally for the slot's current
    /// generation, meant to be visible to a caller deciding whether to stop
    /// trusting this relay.
    pub invalid_signature_count: u64,
}
