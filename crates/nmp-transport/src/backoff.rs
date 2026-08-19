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

/// Production default ceiling for [`jittered`]'s per-URL offset. Anti-
/// thundering-herd jitter is deliberately a FIXED (not re-rolled) value per
/// URL — see [`jittered`]'s doc — so every retry against a given URL pays
/// the same tax until it connects. Real relay outages are measured in
/// seconds, so a `[0, 5s)` spread is negligible there; an in-process test
/// relay that flips ports in milliseconds is exactly where that same tax
/// becomes the dominant (and, per-URL, effectively random) cost. Tests that
/// force a reconnect against a same-process mock relay should override this
/// ceiling low via `PoolConfig::reconnect_jitter_max` rather than padding
/// their own timeout to absorb it.
pub const RECONNECT_JITTER_MAX: Duration = Duration::from_secs(5);

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
/// thunder-herd their reconnects. Same URL always yields the same offset,
/// RE-PAID ON EVERY RETRY against that URL (the offset depends only on
/// `url`, never on the attempt count or `base`) — so a URL unlucky enough
/// to hash near `jitter_max` pays that near-`jitter_max` tax on every
/// single reconnect attempt until one finally succeeds, not just once.
/// `jitter_max` of `Duration::ZERO` disables jitter entirely (returns
/// `base` unchanged) rather than panicking on a degenerate modulus.
#[must_use]
pub fn jittered(base: Duration, url: &str, jitter_max: Duration) -> Duration {
    let bound_ms = u64::try_from(jitter_max.as_millis()).unwrap_or(u64::MAX);
    if bound_ms == 0 {
        return base;
    }
    let hash = url.bytes().fold(0u64, |acc, b| {
        acc.wrapping_mul(31).wrapping_add(u64::from(b))
    });
    let jitter_ms = hash % bound_ms;
    base + Duration::from_millis(jitter_ms)
}

/// HTTP-level denial: the relay's WebSocket handshake explicitly returned
/// 401 or 403. Permanent — the pool must not keep reconnecting on its own;
/// recovery requires an explicit `ensure_open` after the caller addresses
/// the denial.
///
/// Takes the typed HTTP status the handshake actually returned (`None` when
/// the failure never reached one — a bare TCP-connect error, a DNS failure,
/// a stalled TLS/HTTP upgrade, or a post-handshake read error), never a
/// rendered error string. A message built from the relay's own host and
/// port — e.g. `pool::connect::open_relay_socket`'s
/// `"tcp connect {host}:{port}: {error}"` — can contain "401" or "403" for
/// reasons that have nothing to do with an HTTP response (a relay on port
/// 4031, or a hostname with those digits in it), so substring-matching over
/// it is unsound (issue #1788).
#[must_use]
pub fn is_permanent_error(status: Option<u16>) -> bool {
    matches!(status, Some(401 | 403))
}

