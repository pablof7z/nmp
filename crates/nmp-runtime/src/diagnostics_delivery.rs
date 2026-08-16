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
    #[cfg(feature = "bench-instrumentation")]
    let phase_started = Instant::now();
    fan_out(snapshot_with_pool(core, pool), channels);
    #[cfg(feature = "bench-instrumentation")]
    nmp_engine::ingest_attribution::diagnostics_effect(phase_started.elapsed());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::mpsc::{self, TryRecvError};

    use nmp_grammar::{Binding, Filter, LiveQuery};
    use nmp_store::RedbStore;
    use nmp_transport::{PoolConfig, RelayOpenError, RelaySessionKey};
    use nostr::{Keys, RelayUrl};

    fn test_verifier() -> nmp_transport::Verifier {
        nmp_transport::Verifier::new(
            nmp_transport::VerifyConfig::default(),
            std::sync::Arc::new(nmp_transport::NullKnownSig),
        )
        .expect("test verifier construction must succeed")
    }

    use crate::diagnostics_channel::latest_channel;
    use crate::EngineThread;

    #[test]
    fn no_observer_arms_no_work_and_the_first_observed_change_anchors_the_window() {
        let now = Instant::now();
        let first_deadline = now + DELIVERY_WINDOW;
        let mut state = DiagnosticsDeliveryState::default();

        state.changed(now, false);
        assert_eq!(state.next_deadline(), None);

        state.changed(now, true);
        state.changed(now + DELIVERY_WINDOW - Duration::from_millis(1), true);

        assert_eq!(state.next_deadline(), Some(first_deadline));
        assert!(!state.take_due(first_deadline - Duration::from_nanos(1)));
        assert!(state.take_due(first_deadline));
        assert_eq!(state.next_deadline(), None);
    }

    #[test]
    fn eager_delivery_and_last_observer_removal_cancel_pending_work() {
        let now = Instant::now();
        let mut state = DiagnosticsDeliveryState::default();

        state.changed(now, true);
        assert!(state.satisfy());
        assert_eq!(state.next_deadline(), None);

        state.changed(now, true);
        state.clear_if_unobserved(false);
        assert_eq!(state.next_deadline(), None);
    }

    #[test]
    fn one_due_delivery_fans_the_latest_full_snapshot_to_every_observer() {
        let (pool_tx, _pool_rx) = mpsc::channel();
        let pool = Pool::new(
            PoolConfig {
                max_relays: 1,
                ..PoolConfig::default()
            },
            test_verifier(),
            pool_tx,
        )
        .expect("test pool construction");
        let first =
            RelaySessionKey::public(RelayUrl::parse("ws://127.0.0.1:9").expect("first relay URL"));
        let second = RelaySessionKey::public(
            RelayUrl::parse("ws://127.0.0.1:10").expect("second relay URL"),
        );
        pool.ensure_session(&first)
            .expect("first session owns the sole slot");
        assert!(matches!(
            pool.ensure_session(&second),
            Err(RelayOpenError::AtCapacity { max_relays: 1 })
        ));

        let core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 1);
        let (first_tx, first_rx) = latest_channel();
        let (second_tx, second_rx) = latest_channel();
        let channels = HashMap::from([(1, first_tx), (2, second_tx)]);
        let state = RefCell::new(DiagnosticsDeliveryState::default());
        let now = Instant::now();
        state.borrow_mut().changed(now, true);
        state
            .borrow_mut()
            .changed(now + DELIVERY_WINDOW - Duration::from_millis(1), true);

        flush_due(&core, &pool, &channels, &state, now + DELIVERY_WINDOW);

        for receiver in [&first_rx, &second_rx] {
            let snapshot = receiver.recv().expect("one deferred snapshot");
            assert_eq!(snapshot.sessions_rejected_over_cap, 1);
            assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
        }
        pool.shutdown();
    }

    #[test]
    fn new_observer_current_snapshot_satisfies_pending_delivery_without_duplicate() {
        let (pool_tx, _pool_rx) = mpsc::channel();
        let pool = Pool::new(PoolConfig::default(), test_verifier(), pool_tx)
            .expect("test pool construction");
        let core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 1);
        let mut snapshot = snapshot_with_pool(&core, &pool);
        snapshot.uncovered_author_count = 7;

        let (existing_tx, existing_rx) = latest_channel();
        let (new_tx, new_rx) = latest_channel();
        let mut channels = HashMap::from([(1, existing_tx)]);
        let state = RefCell::new(DiagnosticsDeliveryState::default());
        let now = Instant::now();
        state.borrow_mut().changed(now, true);

        seed_observer(snapshot, &new_tx, &channels, &state);
        channels.insert(2, new_tx);

        assert_eq!(state.borrow().next_deadline(), None);
        for receiver in [&existing_rx, &new_rx] {
            assert_eq!(
                receiver
                    .recv()
                    .expect("registration reuses one current snapshot")
                    .uncovered_author_count,
                7
            );
        }

        flush_due(&core, &pool, &channels, &state, now + DELIVERY_WINDOW);
        for receiver in [&existing_rx, &new_rx] {
            assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
        }
        pool.shutdown();
    }

    #[test]
    fn engine_deadline_delivers_the_lazy_withdrawal_snapshot() {
        let (engine, handle) = EngineThread::spawn(
            RedbStore::temporary().expect("temporary Redb store"),
            1,
            PoolConfig::default(),
        )
        .expect("test engine construction");
        let (diagnostics, snapshots) = handle.observe_diagnostics();
        assert_eq!(
            snapshots
                .recv_timeout(Duration::from_secs(1))
                .expect("immediate current snapshot")
                .uncovered_author_count,
            0
        );

        let author = Keys::generate().public_key().to_hex();
        let query = LiveQuery::from_filter(Filter {
            authors: Some(Binding::Literal(BTreeSet::from([author]))),
            ..Filter::default()
        });
        let (query, _rows) = handle.subscribe(query).expect("open observation");
        assert_eq!(
            snapshots
                .recv_timeout(Duration::from_secs(1))
                .expect("open-time diagnostics")
                .uncovered_author_count,
            1
        );

        handle.unsubscribe(query);
        assert_eq!(
            snapshots
                .recv_timeout(Duration::from_secs(1))
                .expect("deferred withdrawal diagnostics")
                .uncovered_author_count,
            0
        );

        diagnostics.cancel();
        handle.shutdown();
        engine.join();
    }
}
