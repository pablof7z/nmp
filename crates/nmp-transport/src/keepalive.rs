//! Relay keepalive FSM (M3 plan §3.2/§4 "Transport pool"). HARVEST source:
//! the old repo's `crates/nmp-network/src/keepalive.rs` — carried over
//! near-verbatim, it is already a clean, transport-agnostic pure FSM with no
//! dependency on the old repo's actor/kernel vocabulary.
//!
//! Per-socket idle-detector that drives WebSocket Ping/Pong without
//! depending on wall-clock time: the worker calls [`KeepaliveState::on_inbound`]
//! whenever it reads from the socket (any frame counts — Pong included),
//! then asks [`KeepaliveState::step`] each tick whether to emit a Ping, give
//! up, or do nothing.
//!
//! Time enters through caller-supplied [`Instant`] only — no
//! `Instant::now()` inside — so this FSM is deterministically testable and
//! the worker integration is a thin wrapper.

use std::time::{Duration, Instant};

/// Production default: emit a ping after this much inbound silence.
pub const KEEPALIVE_IDLE_THRESHOLD: Duration = Duration::from_secs(30);

/// Production default: declare the socket dead if no inbound frame arrives
/// within this window after a ping is emitted.
pub const KEEPALIVE_PONG_TIMEOUT: Duration = Duration::from_secs(30);

/// Verdict returned by [`KeepaliveState::step`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeepaliveAction {
    /// Nothing to do — socket is healthy and not idle past the threshold.
    Idle,
    /// Emit a ping frame on the wire and (once flushed) call
    /// [`KeepaliveState::on_ping_flushed`].
    EmitPing,
    /// Pong window elapsed with no inbound traffic. Caller marks the socket
    /// failed and reconnects.
    Dead,
}

/// Pure per-socket keepalive driver. Owned by the worker loop; reset (a
/// fresh instance) on every reconnect.
pub struct KeepaliveState {
    idle_threshold: Duration,
    pong_timeout: Duration,
    last_inbound_at: Instant,
    ping_sent_at: Option<Instant>,
}

impl KeepaliveState {
    /// Build a fresh driver. `now` is the socket-open moment so the first
    /// `idle_threshold` worth of silence post-connect is tolerated without a
    /// premature ping.
    #[must_use]
    pub fn new(now: Instant, idle_threshold: Duration, pong_timeout: Duration) -> Self {
        Self {
            idle_threshold,
            pong_timeout,
            last_inbound_at: now,
            ping_sent_at: None,
        }
    }

    /// Any inbound frame — including a Pong reply to our Ping. Resets both
    /// the idle clock and any outstanding pong wait. Returns the measured
    /// round-trip if a ping was outstanding (for [`crate::RelayHealth::last_rtt`]).
    pub fn on_inbound(&mut self, now: Instant) -> Option<Duration> {
        self.last_inbound_at = now;
        let rtt = self
            .ping_sent_at
            .map(|sent_at| now.saturating_duration_since(sent_at));
        self.ping_sent_at = None;
        rtt
    }

    /// Step the FSM. Caller supplies `now`; we never read the wall clock.
    pub fn step(&mut self, now: Instant) -> KeepaliveAction {
        if let Some(sent_at) = self.ping_sent_at {
            if now.saturating_duration_since(sent_at) >= self.pong_timeout {
                return KeepaliveAction::Dead;
            }
            return KeepaliveAction::Idle;
        }

        // NOTE: we do NOT stamp `ping_sent_at` here. The pong-timeout clock
        // only starts once the caller confirms the ping actually reached the
        // wire via `on_ping_flushed` — see that method's doc.
        if now.saturating_duration_since(self.last_inbound_at) >= self.idle_threshold {
            return KeepaliveAction::EmitPing;
        }

        KeepaliveAction::Idle
    }

    /// Next wall-clock instant at which [`Self::step`] can change state
    /// without an inbound frame. Callers use this as a blocking deadline,
    /// not a poll rate.
    #[must_use]
    pub fn next_deadline(&self) -> Instant {
        if let Some(sent_at) = self.ping_sent_at {
            sent_at + self.pong_timeout
        } else {
            self.last_inbound_at + self.idle_threshold
        }
    }

    /// Called by the worker after a ping write was confirmed flushed to the
    /// socket. Stamps `ping_sent_at` to the moment the ping actually reached
    /// the wire (not when `step` decided to emit it) — this prevents a
    /// spurious `Dead` verdict under write congestion, where the ping write
    /// is blocked for one or more ticks before it finally flushes.
    pub fn on_ping_flushed(&mut self, now: Instant) {
        if self.ping_sent_at.is_none() {
            self.ping_sent_at = Some(now);
        }
    }

    /// Whether a ping is currently outstanding. Diagnostic-only.
    #[cfg(test)]
    pub(crate) fn ping_in_flight(&self) -> bool {
        self.ping_sent_at.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(secs: u64) -> Duration {
        Duration::from_secs(secs)
    }

    fn fresh() -> (Instant, KeepaliveState) {
        let t0 = Instant::now();
        (t0, KeepaliveState::new(t0, s(30), s(30)))
    }

    #[test]
    fn quiet_socket_emits_ping_after_idle_threshold() {
        let (t0, mut k) = fresh();
        assert_eq!(k.step(t0 + s(29)), KeepaliveAction::Idle);
        assert!(!k.ping_in_flight());
        assert_eq!(k.step(t0 + s(30)), KeepaliveAction::EmitPing);
        assert!(!k.ping_in_flight());
        k.on_ping_flushed(t0 + s(30));
        assert!(k.ping_in_flight());
    }

    #[test]
    fn inbound_resets_idle_clock() {
        let (t0, mut k) = fresh();
        k.on_inbound(t0 + s(25));
        assert_eq!(k.step(t0 + s(30)), KeepaliveAction::Idle);
        assert_eq!(k.step(t0 + s(55)), KeepaliveAction::EmitPing);
    }

    #[test]
    fn ping_in_flight_does_not_re_emit() {
        let (t0, mut k) = fresh();
        assert_eq!(k.step(t0 + s(30)), KeepaliveAction::EmitPing);
        k.on_ping_flushed(t0 + s(30));
        assert_eq!(k.step(t0 + s(35)), KeepaliveAction::Idle);
        assert!(k.ping_in_flight());
    }

    #[test]
    fn blocked_ping_does_not_advance_pong_timeout_clock() {
        let (t0, mut k) = fresh();
        assert_eq!(k.step(t0 + s(30)), KeepaliveAction::EmitPing);
        assert!(!k.ping_in_flight());
        assert_eq!(k.step(t0 + s(31)), KeepaliveAction::EmitPing);
        assert!(!k.ping_in_flight());
        k.on_ping_flushed(t0 + s(32));
        assert!(k.ping_in_flight());
        assert_eq!(k.step(t0 + s(61)), KeepaliveAction::Idle);
        assert_eq!(k.step(t0 + s(62)), KeepaliveAction::Dead);
    }

    #[test]
    fn pong_clears_in_flight_and_resets_idle() {
        let (t0, mut k) = fresh();
        assert_eq!(k.step(t0 + s(30)), KeepaliveAction::EmitPing);
        k.on_ping_flushed(t0 + s(30));
        k.on_inbound(t0 + s(35));
        assert!(!k.ping_in_flight());
        assert_eq!(k.step(t0 + s(64)), KeepaliveAction::Idle);
        assert_eq!(k.step(t0 + s(65)), KeepaliveAction::EmitPing);
    }

    #[test]
    fn pong_timeout_marks_dead() {
        let (t0, mut k) = fresh();
        assert_eq!(k.step(t0 + s(30)), KeepaliveAction::EmitPing);
        k.on_ping_flushed(t0 + s(30));
        assert_eq!(k.step(t0 + s(60)), KeepaliveAction::Dead);
    }

    #[test]
    fn on_ping_flushed_is_idempotent() {
        let (t0, mut k) = fresh();
        assert_eq!(k.step(t0 + s(30)), KeepaliveAction::EmitPing);
        k.on_ping_flushed(t0 + s(30));
        k.on_ping_flushed(t0 + s(35));
        assert_eq!(k.step(t0 + s(59)), KeepaliveAction::Idle);
        assert_eq!(k.step(t0 + s(60)), KeepaliveAction::Dead);
    }
}
