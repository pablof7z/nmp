//! First-change-anchored bounded delivery for lazy diagnostics markers.
//!
//! Diagnostics are latest-state projections, so a burst needs one current
//! snapshot rather than one snapshot per reducer change. This state joins the
//! engine loop's existing event-driven deadline scheduler; it owns no thread
//! and does no work when there is no registered observer.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use nmp_transport::Pool;

use super::diagnostics_channel::LatestSender;
use super::{DiagnosticsSnapshot, EngineCore};

/// One display-frame-sized bound, matching the existing latest-frame delivery
/// cadence. The first change anchors the deadline; later changes join that
/// cohort without extending it.
const DELIVERY_WINDOW: Duration = Duration::from_millis(16);

#[derive(Default)]
pub(super) struct DiagnosticsDeliveryState {
    deadline: Option<Instant>,
}

impl DiagnosticsDeliveryState {
    pub(super) fn changed(&mut self, now: Instant, has_observers: bool) {
        if !has_observers {
            self.deadline = None;
            return;
        }
        if self.deadline.is_none() {
            self.deadline = Some(now + DELIVERY_WINDOW);
        }
    }

    pub(super) fn next_deadline(&self) -> Option<Instant> {
        self.deadline
    }

    fn take_due(&mut self, now: Instant) -> bool {
        if !self.deadline.is_some_and(|deadline| deadline <= now) {
            return false;
        }
        self.deadline = None;
        true
    }

    pub(super) fn satisfy(&mut self) -> bool {
        self.deadline.take().is_some()
    }

    pub(super) fn clear_if_unobserved(&mut self, has_observers: bool) {
        if !has_observers {
            self.deadline = None;
        }
    }
}

pub(super) fn snapshot_with_pool(core: &EngineCore, pool: &Pool) -> DiagnosticsSnapshot {
    let mut snapshot = core.diagnostics_snapshot();
    snapshot.sessions_rejected_over_cap = snapshot
        .sessions_rejected_over_cap
        .saturating_add(pool.admission_rejections());
    snapshot
}

pub(super) fn fan_out(
    snapshot: DiagnosticsSnapshot,
    channels: &HashMap<u64, LatestSender<DiagnosticsSnapshot>>,
) {
    for sender in channels.values() {
        sender.send(snapshot.clone());
    }
}

/// Seed a newly registered observer from the already-materialized current
/// snapshot. When that snapshot also satisfies pending lazy work, reuse it for
/// the existing observers while consuming the deadline; otherwise they would
/// either miss the change or receive a redundant second materialization.
pub(super) fn seed_observer(
    snapshot: DiagnosticsSnapshot,
    sender: &LatestSender<DiagnosticsSnapshot>,
    existing: &HashMap<u64, LatestSender<DiagnosticsSnapshot>>,
    state: &RefCell<DiagnosticsDeliveryState>,
) {
    if state.borrow_mut().satisfy() {
        fan_out(snapshot.clone(), existing);
    }
    sender.send(snapshot);
}

pub(super) fn flush_due(
    core: &EngineCore,
    pool: &Pool,
    channels: &HashMap<u64, LatestSender<DiagnosticsSnapshot>>,
    state: &RefCell<DiagnosticsDeliveryState>,
    now: Instant,
) {
    if !state.borrow_mut().take_due(now) || channels.is_empty() {
        return;
    }
    fan_out(snapshot_with_pool(core, pool), channels);
}

