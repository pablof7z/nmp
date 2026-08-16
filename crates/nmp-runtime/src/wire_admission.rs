//! The 10ms wire-admission window (#1731). Moved out of `lib.rs` beside its
//! sibling owners — see `identity_sessions`'s module doc for why this is a
//! module and not a crate.

use std::time::{Duration, Instant};

const WIRE_ADMISSION_WINDOW: Duration = Duration::from_millis(10);

#[derive(Default)]
pub(super) struct WireAdmissionState {
    deadline: Option<Instant>,
}

impl WireAdmissionState {
    pub(super) fn arm(&mut self, now: Instant) {
        if self.deadline.is_none() {
            self.deadline = Some(now + WIRE_ADMISSION_WINDOW);
        }
    }

    pub(super) fn next_deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub(super) fn take_due(&mut self, now: Instant) -> bool {
        if !self.deadline.is_some_and(|deadline| deadline <= now) {
            return false;
        }
        self.deadline = None;
        true
    }
}

#[cfg(test)]
mod wire_admission_tests {
    use super::*;

    #[test]
    fn window_is_anchored_to_first_arrival_and_rearms_for_the_next_cohort() {
        assert_eq!(WIRE_ADMISSION_WINDOW, Duration::from_millis(10));
        let now = Instant::now();
        let first_deadline = now + WIRE_ADMISSION_WINDOW;
        let mut state = WireAdmissionState::default();

        state.arm(now);
        state.arm(now + WIRE_ADMISSION_WINDOW - Duration::from_millis(1));

        assert_eq!(state.next_deadline(), Some(first_deadline));
        assert!(!state.take_due(first_deadline - Duration::from_nanos(1)));
        assert!(state.take_due(first_deadline));
        assert_eq!(state.next_deadline(), None);

        state.arm(first_deadline + Duration::from_millis(1));
        assert_eq!(
            state.next_deadline(),
            Some(first_deadline + Duration::from_millis(1) + WIRE_ADMISSION_WINDOW)
        );
    }
}
