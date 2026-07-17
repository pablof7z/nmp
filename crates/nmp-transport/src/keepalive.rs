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

use std::time::{Duration, Instant, SystemTime};

/// Production default: emit a ping after this much inbound silence.
pub const KEEPALIVE_IDLE_THRESHOLD: Duration = Duration::from_secs(30);

/// Production default: declare the socket dead if no inbound frame arrives
/// within this window after a ping is emitted.
pub const KEEPALIVE_PONG_TIMEOUT: Duration = Duration::from_secs(30);

/// Threshold for [`SuspendGapDetector`]: a gap this large between two
/// consecutive worker-loop iterations, measured on a clock that keeps
/// advancing while the process is suspended, means the process itself was
/// frozen (e.g. iOS backgrounding) rather than merely idle. Comfortably
/// above the worst-case ordinary inter-iteration wait — bounded by
/// [`KEEPALIVE_IDLE_THRESHOLD`]/[`KEEPALIVE_PONG_TIMEOUT`], each 30s — so a
/// healthy idle relay never trips it, and far below any real suspend
/// interval (iOS backgrounding is measured in seconds to minutes before the
/// socket is killed and the process frozen).
pub const SUSPEND_GAP_THRESHOLD: Duration = Duration::from_secs(45);

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

    /// Whether a ping is currently outstanding. Used by the resume-gap
    /// heuristic (see [`SuspendGapDetector`]) to avoid emitting a second
    /// ping on top of one already awaiting its pong, and by tests as a
    /// diagnostic.
    pub(crate) fn ping_in_flight(&self) -> bool {
        self.ping_sent_at.is_some()
    }
}

/// Detects a resume-after-suspend gap between consecutive worker-loop
/// iterations (issue #4). Callers supply a wall-clock (or otherwise
/// sleep-advancing) reading at each iteration; this struct never reads a
/// clock itself, mirroring [`KeepaliveState`]'s injected-time design so it
/// stays deterministically testable.
///
/// CRITICAL: on Apple platforms, `std::time::Instant` is backed by
/// `CLOCK_UPTIME_RAW`, which does **not** advance while the device sleeps —
/// two `Instant::now()` reads taken immediately before suspend and
/// immediately after resume can show near-zero elapsed time even though
/// wall-clock minutes passed. `KeepaliveState` above is intentionally kept
/// on `Instant` (it drives a pure, deterministic FSM and its correctness
/// does not depend on wall-clock accounting: the worker thread itself is
/// frozen for the same interval `Instant` fails to observe, so nothing
/// running on that clock inside one suspended process ever observes a
/// skewed relative duration). Detecting THAT a suspension happened at all —
/// which is the whole point here — requires a clock that keeps advancing
/// across it: `std::time::SystemTime` (wall clock) or a platform continuous
/// monotonic clock. `SystemTime` is used here; a backward jump (e.g. an NTP
/// correction) is treated as no gap rather than a spurious huge one.
pub struct SuspendGapDetector {
    last_seen: SystemTime,
    threshold: Duration,
}

impl SuspendGapDetector {
    /// Build a fresh detector. `now` is the first iteration's wall-clock
    /// reading.
    #[must_use]
    pub fn new(now: SystemTime, threshold: Duration) -> Self {
        Self {
            last_seen: now,
            threshold,
        }
    }

    /// Record one worker-loop iteration's wall-clock reading. Returns `true`
    /// when the elapsed time since the previous call reached the threshold —
    /// the caller's signal to accelerate keepalive (see
    /// [`SUSPEND_GAP_THRESHOLD`]'s doc). Always updates the internal
    /// reading, gap or not, so the next call measures from here.
    pub fn observe(&mut self, now: SystemTime) -> bool {
        let gap = now.duration_since(self.last_seen).unwrap_or(Duration::ZERO);
        self.last_seen = now;
        gap >= self.threshold
    }
}

/// Combine one loop iteration's [`SuspendGapDetector::observe`] result with
/// [`KeepaliveState::step`]'s verdict: a detected gap upgrades an `Idle`
/// verdict to `EmitPing` so a dead/half-open socket is probed immediately
/// on resume instead of waiting out the remaining idle threshold. A ping
/// already in flight is left alone -- that case is already governed by the
/// existing pong-timeout clock, and must never receive a second ping on
/// top of it. `Dead` passes through unchanged (already worse than a ping).
#[must_use]
pub(crate) fn apply_resume_gap(
    action: KeepaliveAction,
    ping_in_flight: bool,
    gap_detected: bool,
) -> KeepaliveAction {
    if gap_detected && !ping_in_flight && action == KeepaliveAction::Idle {
        KeepaliveAction::EmitPing
    } else {
        action
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

    #[test]
    fn suspend_gap_detector_reports_no_gap_under_threshold() {
        let t0 = SystemTime::now();
        let mut detector = SuspendGapDetector::new(t0, s(45));
        assert!(!detector.observe(t0 + s(10)));
        assert!(!detector.observe(t0 + s(20)));
        assert!(!detector.observe(t0 + s(44)));
    }

    #[test]
    fn suspend_gap_detector_reports_gap_at_or_over_threshold() {
        let t0 = SystemTime::now();
        let mut detector = SuspendGapDetector::new(t0, s(45));
        assert!(detector.observe(t0 + s(45)));
    }

    #[test]
    fn suspend_gap_detector_reports_large_gap_after_simulated_suspend() {
        let t0 = SystemTime::now();
        let mut detector = SuspendGapDetector::new(t0, s(45));
        // A device backgrounded for 10+ minutes: exactly the acceptance
        // scenario in issue #4.
        assert!(detector.observe(t0 + Duration::from_secs(600)));
    }

    #[test]
    fn suspend_gap_detector_measures_from_the_previous_observation_not_the_start() {
        let t0 = SystemTime::now();
        let mut detector = SuspendGapDetector::new(t0, s(45));
        assert!(!detector.observe(t0 + s(40)));
        // Only 10s elapsed since the last observation (t0+40), even though
        // 50s elapsed since construction -- must not falsely report a gap.
        assert!(!detector.observe(t0 + s(50)));
    }

    #[test]
    fn suspend_gap_detector_treats_backward_clock_jump_as_no_gap() {
        let t0 = SystemTime::now();
        let mut detector = SuspendGapDetector::new(t0, s(45));
        // An NTP correction moving the wall clock backward must never be
        // reported as a (nonsensical negative) gap.
        assert!(!detector.observe(t0 - s(120)));
    }

    #[test]
    fn apply_resume_gap_upgrades_idle_to_ping_only_when_gap_and_no_ping_in_flight() {
        assert_eq!(
            apply_resume_gap(KeepaliveAction::Idle, false, true),
            KeepaliveAction::EmitPing,
            "a resume gap with no outstanding ping must trigger an immediate ping"
        );
    }

    #[test]
    fn apply_resume_gap_is_inert_without_a_gap() {
        assert_eq!(
            apply_resume_gap(KeepaliveAction::Idle, false, false),
            KeepaliveAction::Idle,
            "no gap observed must never produce an early ping"
        );
    }

    #[test]
    fn apply_resume_gap_never_double_pings_an_outstanding_ping() {
        assert_eq!(
            apply_resume_gap(KeepaliveAction::Idle, true, true),
            KeepaliveAction::Idle,
            "a ping already awaiting its pong must not receive a second one"
        );
    }

    #[test]
    fn apply_resume_gap_leaves_emit_ping_unchanged() {
        assert_eq!(
            apply_resume_gap(KeepaliveAction::EmitPing, false, true),
            KeepaliveAction::EmitPing
        );
    }

    #[test]
    fn apply_resume_gap_leaves_dead_unchanged() {
        assert_eq!(
            apply_resume_gap(KeepaliveAction::Dead, false, true),
            KeepaliveAction::Dead,
            "a gap must never mask an already-Dead verdict"
        );
    }
}
