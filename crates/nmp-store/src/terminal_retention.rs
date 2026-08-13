use std::time::{SystemTime, UNIX_EPOCH};

use nostr::Timestamp;

/// Terminal receipt history is internal operational evidence, not an app
/// policy. These three limits are deliberately fixed together: at the
/// production Mosaico rate (about 77,760 closures/day) and observed mean
/// closure size (about 1.8 KiB), one day is roughly 134 MiB.
pub(crate) const TERMINAL_RECEIPT_MAX_AGE_SECS: u64 = 24 * 60 * 60;
pub(crate) const TERMINAL_RECEIPT_MAX_COUNT: u64 = 100_000;
pub(crate) const TERMINAL_RECEIPT_MAX_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub(crate) struct TerminalRetentionLimits {
    pub(crate) max_age_secs: u64,
    pub(crate) max_count: u64,
    pub(crate) max_bytes: u64,
}

impl TerminalRetentionLimits {
    pub(crate) const PRODUCTION: Self = Self {
        max_age_secs: TERMINAL_RECEIPT_MAX_AGE_SECS,
        max_count: TERMINAL_RECEIPT_MAX_COUNT,
        max_bytes: TERMINAL_RECEIPT_MAX_BYTES,
    };
}

pub(crate) fn wall_clock_now() -> Timestamp {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    Timestamp::from(seconds)
}
