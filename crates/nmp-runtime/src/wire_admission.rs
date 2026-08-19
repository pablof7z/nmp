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

