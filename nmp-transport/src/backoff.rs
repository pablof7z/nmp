//! Reconnect backoff schedule + per-URL jitter (M3 plan §3.2/§4 "Transport
//! pool"). HARVEST source: the old repo's
//! `crates/nmp-network/src/relay_protocol.rs` (`RELAY_RECONNECT_DELAY_*`,
//! `jittered_backoff`, the V-92 healthy-session reset in
//! `apply_reconnect_backoff`). The `BackoffClass`/rate-limit-hint plumbing
//! (V-58) is dropped — that was a kernel-driven diagnostic hint with no
//! reader in the M3 two-noun surface; the exponential curve + jitter +
//! healthy-session reset are the load-bearing operational lessons and are
//! kept.
//!
//! Pure: no `Instant::now()` in here. The worker supplies elapsed durations;
//! this module is a plain, deterministically-testable step function.

use std::time::Duration;

/// Initial mid-session reconnect delay. Doubled on each consecutive failure
/// up to [`RECONNECT_DELAY_MAX`]; reset to this value once a connection has
/// stayed healthy for [`BACKOFF_RESET_AFTER`].
pub const RECONNECT_DELAY_INITIAL: Duration = Duration::from_secs(3);

/// Upper bound on the exponential reconnect-delay growth.
pub const RECONNECT_DELAY_MAX: Duration = Duration::from_secs(300);

/// After a relay has been connected for this duration, the reconnect backoff
/// resets to [`RECONNECT_DELAY_INITIAL`] on the next disconnect (harvested
/// V-92 lesson: a relay that was healthy for a long session shouldn't inherit
/// a maxed-out backoff from a stale prior failure streak).
pub const BACKOFF_RESET_AFTER: Duration = Duration::from_secs(300);

/// Advance the exponential backoff schedule for one disconnect and return the
/// (pre-jitter) delay to wait before the next reconnect attempt.
///
/// `current` is mutated in place so the next call continues from the updated
/// value. `connected_for` is `None` for a connect-time failure (never
/// reached a live session) and `Some(elapsed)` for a mid-session drop, where
/// `elapsed` is how long the socket was up before it dropped.
pub fn advance(current: &mut Duration, connected_for: Option<Duration>) -> Duration {
    let stayed_healthy = connected_for.is_some_and(|d| d >= BACKOFF_RESET_AFTER);
    if stayed_healthy {
        *current = RECONNECT_DELAY_INITIAL;
    } else {
        *current = (*current * 2).min(RECONNECT_DELAY_MAX);
    }
    *current
}

/// Per-URL deterministic jitter so simultaneously-failing relays don't
/// thunder-herd their reconnects. Same URL always yields the same offset;
/// distinct URLs spread across a `[0, 5s)` window.
#[must_use]
pub fn jittered(base: Duration, url: &str) -> Duration {
    let hash = url.bytes().fold(0u64, |acc, b| {
        acc.wrapping_mul(31).wrapping_add(u64::from(b))
    });
    let jitter_ms = hash % 5000;
    base + Duration::from_millis(jitter_ms)
}

/// HTTP-level denial: the relay explicitly rejected the connection (401/403).
/// Permanent — the pool must not keep reconnecting on its own; recovery
/// requires an explicit `ensure_open` after the caller addresses the denial.
#[must_use]
pub fn is_permanent_error(error: &str) -> bool {
    error.contains("403") || error.contains("401") || error.contains("Forbidden")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advance_doubles_up_to_max() {
        let mut backoff = RECONNECT_DELAY_INITIAL;
        assert_eq!(advance(&mut backoff, None), Duration::from_secs(6));
        assert_eq!(advance(&mut backoff, None), Duration::from_secs(12));
        assert_eq!(advance(&mut backoff, None), Duration::from_secs(24));
    }

    #[test]
    fn advance_caps_at_max() {
        let mut backoff = RECONNECT_DELAY_MAX;
        assert_eq!(advance(&mut backoff, None), RECONNECT_DELAY_MAX);
    }

    #[test]
    fn advance_resets_after_healthy_session() {
        let mut backoff = RECONNECT_DELAY_MAX;
        let delay = advance(&mut backoff, Some(BACKOFF_RESET_AFTER));
        assert_eq!(delay, RECONNECT_DELAY_INITIAL);
    }

    #[test]
    fn advance_does_not_reset_short_session() {
        let mut backoff = RECONNECT_DELAY_INITIAL;
        let delay = advance(&mut backoff, Some(Duration::from_secs(1)));
        assert_eq!(delay, Duration::from_secs(6));
    }

    #[test]
    fn jittered_backoff_is_deterministic_per_url() {
        let base = Duration::from_secs(3);
        assert_eq!(
            jittered(base, "wss://relay.example"),
            jittered(base, "wss://relay.example")
        );
    }

    #[test]
    fn jittered_backoff_spreads_across_distinct_urls() {
        let base = Duration::from_secs(3);
        let urls = ["wss://a.example", "wss://b.example", "wss://c.example"];
        let offsets: std::collections::HashSet<_> =
            urls.iter().map(|u| jittered(base, u) - base).collect();
        assert!(offsets.len() >= 2, "expected spread, got {offsets:?}");
    }

    #[test]
    fn jittered_backoff_bounded_by_five_seconds() {
        let base = Duration::from_secs(3);
        for url in ["wss://r.example", "wss://very-long.example/path", ""] {
            let delay = jittered(base, url);
            assert!(delay >= base && delay < base + Duration::from_secs(5));
        }
    }

    #[test]
    fn is_permanent_error_matches_documented_codes() {
        assert!(is_permanent_error("401 Unauthorized"));
        assert!(is_permanent_error("403 Forbidden"));
        assert!(is_permanent_error("Forbidden — bring NIP-42"));
        assert!(!is_permanent_error("502 Bad Gateway"));
        assert!(!is_permanent_error("connection reset by peer"));
    }
}
