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
    /// Count of TEXT frames from this relay that did not decode into a
    /// `RelayMessage` at all (`pool::frame::classify_text_with_candidate`).
    ///
    /// Deliberately separate from [`Self::invalid_signature_count`]: an
    /// undecodable frame is not a forgery, and folding it into the
    /// misbehavior tally would make a relay emitting one stray malformed
    /// line look like one serving forged events. It carries no subscription
    /// id — the text that would have named one is the text that failed to
    /// parse — so it is a fact about the SESSION and nothing narrower.
    ///
    /// A caller counting what a relay returned needs this: an undecodable
    /// frame may have been an EVENT for any request on the session, so a
    /// nonzero count here means no request's returned-frame total is exact
    /// (#1668). Like `invalid_signature_count`, a lifetime tally for the
    /// slot's current generation, never cleared by a reconnect.
    pub undecodable_frame_count: u64,
}

impl RelayHealth {
    /// Record one text frame that did not decode into a `RelayMessage`.
    pub(crate) fn record_undecodable_frame(&mut self) {
        self.undecodable_frame_count = self.undecodable_frame_count.saturating_add(1);
    }
}
