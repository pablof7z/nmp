//! The exponential store-recovery backoff schedule (#1731). Moved out of
//! `lib.rs` beside its sibling owners — see `identity_sessions`'s module doc
//! for why this is a module and not a crate.

use std::time::{Duration, Instant};

const STORE_RECOVERY_INITIAL_BACKOFF: Duration = Duration::from_millis(100);
const STORE_RECOVERY_MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Runtime-owned retry schedule for one failed durable-store generation.
/// `next_attempt: Some` is the complete retry lifecycle state. `None` means
/// this driver owns no retry; the core's typed diagnostic separately says
/// whether that is because the store is healthy or the fault is permanent.
#[derive(Default)]
pub(super) struct StoreRecoveryDriver {
    next_attempt: Option<Instant>,
    failures: u32,
}

impl StoreRecoveryDriver {
    pub(super) fn arm_now(&mut self, now: Instant) {
        self.next_attempt.get_or_insert(now);
    }

    pub(super) fn is_due(&self, now: Instant) -> bool {
        self.next_attempt.is_some_and(|deadline| deadline <= now)
    }

    pub(super) fn wait(&self, now: Instant) -> Option<Duration> {
        self.next_attempt
            .map(|deadline| deadline.saturating_duration_since(now))
    }

    pub(super) fn record_failure(&mut self, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        let shift = self.failures.saturating_sub(1).min(9);
        let multiplier = 1u32 << shift;
        let delay = STORE_RECOVERY_INITIAL_BACKOFF
            .saturating_mul(multiplier)
            .min(STORE_RECOVERY_MAX_BACKOFF);
        self.next_attempt = Some(now + delay);
    }

    pub(super) fn recovered(&mut self) {
        self.next_attempt = None;
        self.failures = 0;
    }

    pub(super) fn stop_retrying(&mut self) {
        self.next_attempt = None;
        self.failures = 0;
    }

    pub(super) fn is_active(&self) -> bool {
        self.next_attempt.is_some()
    }
}

#[cfg(test)]
mod store_recovery_driver_tests {
    use super::*;

    #[test]
    fn recovery_backoff_is_exponential_event_driven_and_capped() {
        let now = Instant::now();
        let mut driver = StoreRecoveryDriver::default();
        driver.arm_now(now);
        assert!(driver.is_due(now));

        for expected_millis in [
            100_u64, 200, 400, 800, 1_600, 3_200, 6_400, 12_800, 25_600, 30_000, 30_000,
        ] {
            driver.record_failure(now);
            assert_eq!(
                driver.wait(now),
                Some(Duration::from_millis(expected_millis))
            );
            assert!(!driver.is_due(now));
        }

        driver.recovered();
        assert!(!driver.is_active());
        assert_eq!(driver.wait(now), None);
    }
}
