//! Bounded ordinary-row delivery (#46).
//!
//! Reducer emits are exact deltas, but an unbounded `mpsc` queue lets a slow
//! observer retain every intermediate batch. This channel instead keeps one
//! pending transition per event id in one mailbox slot. Each new reducer
//! delta is composed onto that transition atomically. Applying the batch the
//! receiver gets to its last delivered state therefore produces the newest
//! reducer state even when intermediate emits were skipped.
//!
//! This is not full-set snapshot redelivery: unchanged rows are absent, so a
//! growing query does not regain the O(rows squared) behavior that incremental
//! deltas removed. Memory is bounded by the difference between the receiver's
//! last delivered state and the current state, plus one in-flight callback
//! batch.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::marker::PhantomData;
use std::sync::mpsc::{RecvError, RecvTimeoutError, TryRecvError};
use std::sync::Mutex;
use std::time::Duration;

use nostr::{EventId, RelayUrl};

use crate::core::{AcquisitionEvidence, ObservationEvidence, ObservationFact, Row, RowDelta};

use super::diagnostics_channel::{
    latest_channel, AsyncLatestReceiver, ConcurrentNext, LatestReceiver, LatestSender,
};
use super::RowsMsg;

enum PendingTransition {
    /// Absent at the receiver's baseline, present now.
    Added(Row),
    /// Present at the baseline and now, with the latest complete source set.
    SourcesGrew(BTreeSet<RelayUrl>),
    /// Present at the baseline, absent now.
    Removed,
    /// Present at the baseline and now, but removed and re-added in between.
    /// The remove/add pair carries a complete current row without retaining
    /// the receiver's old row in the producer mailbox.
    Replaced(Row),
}

struct PendingRows {
    by_id: BTreeMap<EventId, PendingTransition>,
    evidence: AcquisitionEvidence,
    execution: VecDeque<ObservationEvidence>,
}

const EXECUTION_EVIDENCE_CAPACITY: usize = 256;

impl PendingRows {
    fn new(evidence: AcquisitionEvidence) -> Self {
        Self {
            by_id: BTreeMap::new(),
            evidence,
            execution: VecDeque::new(),
        }
    }

    fn push_execution(&mut self, facts: Vec<ObservationEvidence>) {
        self.execution.extend(facts);
        if self.execution.len() <= EXECUTION_EVIDENCE_CAPACITY {
            return;
        }

        let mut first = u64::MAX;
        let mut last = 0;
        let mut dropped = 0u64;
        while self
            .execution
            .front()
            .is_some_and(|fact| matches!(fact.fact, ObservationFact::Overflow { .. }))
        {
            let prior = self.execution.pop_front().expect("front existed");
            if let ObservationFact::Overflow {
                first_sequence,
                last_sequence,
                dropped: prior_dropped,
            } = prior.fact
            {
                first = first.min(first_sequence);
                last = last.max(last_sequence);
                dropped = dropped.saturating_add(prior_dropped);
            }
        }
        while self.execution.len() >= EXECUTION_EVIDENCE_CAPACITY {
            let removed = self.execution.pop_front().expect("length checked");
            first = first.min(removed.sequence);
            last = last.max(removed.sequence);
            dropped = dropped.saturating_add(1);
        }
        self.execution.push_front(ObservationEvidence {
            sequence: last,
            fact: ObservationFact::Overflow {
                first_sequence: first,
                last_sequence: last,
                dropped,
            },
        });
    }

    fn push(&mut self, delta: RowDelta) {
        let id = delta.id();
        let previous = self.by_id.remove(&id);
        let next = match (previous, delta) {
            (None, RowDelta::Added(row)) => Some(PendingTransition::Added(row)),
            (None, RowDelta::SourcesGrew { sources, .. }) => {
                Some(PendingTransition::SourcesGrew(sources))
            }
            (None, RowDelta::Removed(_)) => Some(PendingTransition::Removed),

            (Some(PendingTransition::Added(_)), RowDelta::Added(row)) => {
                Some(PendingTransition::Added(row))
            }
            (Some(PendingTransition::Added(mut row)), RowDelta::SourcesGrew { sources, .. }) => {
                row.sources = sources;
                Some(PendingTransition::Added(row))
            }
            (Some(PendingTransition::Added(_)), RowDelta::Removed(_)) => None,

            (Some(PendingTransition::SourcesGrew(_)), RowDelta::Added(row)) => {
                Some(PendingTransition::SourcesGrew(row.sources))
            }
            (Some(PendingTransition::SourcesGrew(_)), RowDelta::SourcesGrew { sources, .. }) => {
                Some(PendingTransition::SourcesGrew(sources))
            }
            (Some(PendingTransition::SourcesGrew(_)), RowDelta::Removed(_)) => {
                Some(PendingTransition::Removed)
            }

            (Some(PendingTransition::Removed), RowDelta::Added(row)) => {
                Some(PendingTransition::Replaced(row))
            }
            // `SourcesGrew` is legal only while the row remains present. Once
            // this pending transition has removed it, a source-only delta
            // cannot prove presence again because it deliberately carries no
            // row payload. Preserve the removal rather than resurrecting the
            // receiver's stale baseline row if an upstream invariant breaks.
            (Some(PendingTransition::Removed), RowDelta::SourcesGrew { .. }) => {
                Some(PendingTransition::Removed)
            }
            (Some(PendingTransition::Removed), RowDelta::Removed(_)) => {
                Some(PendingTransition::Removed)
            }

            (Some(PendingTransition::Replaced(_)), RowDelta::Added(row)) => {
                Some(PendingTransition::Replaced(row))
            }
            (Some(PendingTransition::Replaced(mut row)), RowDelta::SourcesGrew { sources, .. }) => {
                row.sources = sources;
                Some(PendingTransition::Replaced(row))
            }
            (Some(PendingTransition::Replaced(_)), RowDelta::Removed(_)) => {
                Some(PendingTransition::Removed)
            }
        };
        if let Some(next) = next {
            self.by_id.insert(id, next);
        }
    }

    fn into_message(self) -> RowsMsg {
        let mut deltas = Vec::with_capacity(self.by_id.len());
        for (id, transition) in self.by_id {
            match transition {
                PendingTransition::Added(row) => deltas.push(RowDelta::Added(row)),
                PendingTransition::SourcesGrew(sources) => {
                    deltas.push(RowDelta::SourcesGrew { id, sources });
                }
                PendingTransition::Removed => deltas.push(RowDelta::Removed(id)),
                PendingTransition::Replaced(row) => {
                    deltas.push(RowDelta::Removed(id));
                    deltas.push(RowDelta::Added(row));
                }
            }
        }
        (deltas, self.evidence, self.execution.into_iter().collect())
    }
}

pub(crate) struct RowsSender {
    pending: LatestSender<PendingRows>,
    /// The acquisition snapshot most recently sent for this observation.
    ///
    /// Execution-only facts can arrive after the receiver consumed the latest
    /// row batch. Retaining this snapshot prevents those facts from replacing
    /// real acquisition evidence with `AcquisitionEvidence::default()`.
    last_evidence: Mutex<AcquisitionEvidence>,
}

/// The single-consumer half of an ordinary live-query stream.
///
/// At most one exact rebased transition is pending. A slow consumer can skip
/// intermediate reducer emits, but applying its next batch to the state from
/// its previous return always yields the newest reducer state. Like
/// `std::sync::mpsc::Receiver`, this value is `Send` but deliberately not
/// `Sync`.
///
/// ```compile_fail
/// use nmp_engine::runtime::RowsReceiver;
/// fn require_sync<T: Sync>() {}
/// require_sync::<RowsReceiver>();
/// ```
pub struct RowsReceiver {
    pending: LatestReceiver<PendingRows>,
    not_sync: PhantomData<Cell<()>>,
}

pub(crate) fn rows_channel() -> (RowsSender, RowsReceiver) {
    let (sender, receiver) = latest_channel();
    (
        RowsSender {
            pending: sender,
            last_evidence: Mutex::new(AcquisitionEvidence::default()),
        },
        RowsReceiver {
            pending: receiver,
            not_sync: PhantomData,
        },
    )
}

impl RowsSender {
    pub(crate) fn send(&self, (deltas, evidence, execution): RowsMsg) {
        #[cfg(feature = "bench-instrumentation")]
        let send_started = std::time::Instant::now();
        #[cfg(feature = "bench-instrumentation")]
        let delta_count = deltas.len();
        *self.last_evidence.lock().unwrap() = evidence.clone();
        self.pending.update(|pending| {
            let pending = pending.get_or_insert_with(|| PendingRows::new(evidence.clone()));
            for delta in deltas {
                pending.push(delta);
            }
            pending.evidence = evidence;
            pending.push_execution(execution);
        });
        #[cfg(feature = "bench-instrumentation")]
        crate::ingest_attribution::row_channel_send(send_started.elapsed(), delta_count);
    }

    pub(crate) fn send_evidence(&self, execution: Vec<ObservationEvidence>) {
        let evidence = self.last_evidence.lock().unwrap().clone();
        self.pending.update(|pending| {
            let pending = pending.get_or_insert_with(|| PendingRows::new(evidence));
            pending.push_execution(execution);
        });
    }
}

impl RowsReceiver {
    pub fn recv(&self) -> Result<RowsMsg, RecvError> {
        self.pending
            .recv()
            .map(PendingRows::into_message)
            .ok_or(RecvError)
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<RowsMsg, RecvTimeoutError> {
        self.pending
            .recv_timeout(timeout)
            .map(PendingRows::into_message)
    }

    pub fn try_recv(&self) -> Result<RowsMsg, TryRecvError> {
        self.pending.try_recv().map(PendingRows::into_message)
    }

    /// Convert to the `Send + Sync` async pull surface (#680). Consumes the
    /// blocking receiver — a stream is drained either by a direct-Rust blocking
    /// consumer or by an async foreign consumer, never both.
    pub fn into_async(self) -> AsyncRowsReceiver {
        AsyncRowsReceiver {
            pending: AsyncLatestReceiver::new(self.pending),
        }
    }
}

/// The async single-consumer half of an ordinary live-query stream (#680).
/// Awaiting [`Self::next`] parks a waker on the mailbox rather than blocking an
/// OS thread; the fold that keeps exactly one pending exact transition is
/// entirely sender-side, so this receiver carries no per-frame state and is
/// `Send + Sync`.
pub struct AsyncRowsReceiver {
    pending: AsyncLatestReceiver<PendingRows>,
}

impl AsyncRowsReceiver {
    /// Await the next exact rebased transition, or `None` once the producer is
    /// gone / the consumer cancelled. [`ConcurrentNext`] on an overlapping call.
    pub async fn next(&self) -> Result<Option<RowsMsg>, ConcurrentNext> {
        Ok(self.pending.next().await?.map(PendingRows::into_message))
    }

    /// Idempotent consumer-initiated close; wakes a parked `next()` to `None`.
    pub fn close(&self) {
        self.pending.close();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::time::Duration;

    use nostr::{Keys, Kind, Timestamp, UnsignedEvent};

    use super::*;
    use crate::core::ShortfallFact;

    fn row(keys: &Keys, created_at: u64, content: &str) -> Row {
        Row {
            event: UnsignedEvent::new(
                keys.public_key(),
                Timestamp::from(created_at),
                Kind::TextNote,
                Vec::new(),
                content,
            )
            .sign_with_keys(keys)
            .unwrap(),
            sources: BTreeSet::new(),
        }
    }

    fn apply(rows: &mut BTreeMap<EventId, Row>, deltas: &[RowDelta]) {
        for delta in deltas {
            match delta {
                RowDelta::Added(row) => {
                    rows.insert(row.event.id, row.clone());
                }
                RowDelta::SourcesGrew { id, sources } => {
                    rows.get_mut(id).unwrap().sources = sources.clone();
                }
                RowDelta::Removed(id) => {
                    rows.remove(id);
                }
            }
        }
    }

    fn latest_evidence() -> AcquisitionEvidence {
        AcquisitionEvidence {
            sources: Vec::new(),
            shortfall: vec![ShortfallFact::NoResolvedDemand],
        }
    }

    fn send_rows(tx: &RowsSender, deltas: Vec<RowDelta>, evidence: AcquisitionEvidence) {
        tx.send((deltas, evidence, Vec::new()));
    }

    #[test]
    fn ten_thousand_skipped_updates_form_one_exact_transition() {
        fn assert_send<T: Send>() {}
        assert_send::<RowsReceiver>();

        let keys = Keys::generate();
        let mut expected = row(&keys, 1, "same-event");
        let id = expected.event.id;
        let (tx, rx) = rows_channel();
        send_rows(
            &tx,
            vec![RowDelta::Added(expected.clone())],
            AcquisitionEvidence::default(),
        );
        let mut delivered = BTreeMap::new();
        apply(&mut delivered, &rx.recv().unwrap().0);

        for update in 0..5_000 {
            send_rows(
                &tx,
                vec![RowDelta::Removed(id)],
                AcquisitionEvidence::default(),
            );
            expected.sources = [RelayUrl::parse(&format!("wss://r{update}.example")).unwrap()]
                .into_iter()
                .collect();
            send_rows(
                &tx,
                vec![RowDelta::Added(expected.clone())],
                AcquisitionEvidence::default(),
            );
        }

        let (deltas, _, _) = rx.recv().unwrap();
        assert_eq!(deltas.len(), 2, "one remove/add transition for one id");
        apply(&mut delivered, &deltas);
        assert_eq!(delivered.get(&id), Some(&expected));
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn add_then_remove_cancels_but_latest_evidence_is_delivered() {
        let keys = Keys::generate();
        let added = row(&keys, 1, "temporary");
        let (tx, rx) = rows_channel();
        send_rows(
            &tx,
            vec![RowDelta::Added(added.clone())],
            AcquisitionEvidence::default(),
        );
        let evidence = latest_evidence();
        send_rows(
            &tx,
            vec![RowDelta::Removed(added.event.id)],
            evidence.clone(),
        );

        let (deltas, received_evidence, _) = rx.recv().unwrap();
        assert!(deltas.is_empty());
        assert_eq!(received_evidence, evidence);
    }

    #[test]
    fn source_growth_keeps_only_the_latest_complete_source_set() {
        let keys = Keys::generate();
        let initial = row(&keys, 1, "provenance");
        let id = initial.event.id;
        let a = RelayUrl::parse("wss://a.example").unwrap();
        let b = RelayUrl::parse("wss://b.example").unwrap();
        let (tx, rx) = rows_channel();
        send_rows(
            &tx,
            vec![RowDelta::Added(initial)],
            AcquisitionEvidence::default(),
        );
        rx.recv().unwrap();
        send_rows(
            &tx,
            vec![RowDelta::SourcesGrew {
                id,
                sources: [a.clone()].into_iter().collect(),
            }],
            AcquisitionEvidence::default(),
        );
        let expected: BTreeSet<_> = [a, b].into_iter().collect();
        send_rows(
            &tx,
            vec![RowDelta::SourcesGrew {
                id,
                sources: expected.clone(),
            }],
            latest_evidence(),
        );

        let (deltas, evidence, _) = rx.recv().unwrap();
        assert!(matches!(
            deltas.as_slice(),
            [RowDelta::SourcesGrew { id: delta_id, sources }]
                if *delta_id == id && sources == &expected
        ));
        assert_eq!(evidence, latest_evidence());
    }

    #[test]
    fn source_growth_after_removal_fails_closed_without_resurrecting_the_row() {
        let keys = Keys::generate();
        let initial = row(&keys, 1, "must-stay-removed");
        let id = initial.event.id;
        let (tx, rx) = rows_channel();
        send_rows(
            &tx,
            vec![RowDelta::Added(initial)],
            AcquisitionEvidence::default(),
        );
        let mut delivered = BTreeMap::new();
        apply(&mut delivered, &rx.recv().unwrap().0);

        send_rows(
            &tx,
            vec![RowDelta::Removed(id)],
            AcquisitionEvidence::default(),
        );
        let evidence = latest_evidence();
        send_rows(
            &tx,
            vec![RowDelta::SourcesGrew {
                id,
                sources: [RelayUrl::parse("wss://unexpected.example").unwrap()]
                    .into_iter()
                    .collect(),
            }],
            evidence.clone(),
        );

        let (deltas, received_evidence, _) = rx.recv().unwrap();
        assert!(matches!(deltas.as_slice(), [RowDelta::Removed(delta_id)] if *delta_id == id));
        apply(&mut delivered, &deltas);
        assert!(!delivered.contains_key(&id));
        assert_eq!(received_evidence, evidence);
    }

    #[test]
    fn pending_transition_is_delivered_before_disconnect() {
        let keys = Keys::generate();
        let added = row(&keys, 1, "last");
        let (tx, rx) = rows_channel();
        send_rows(
            &tx,
            vec![RowDelta::Added(added)],
            AcquisitionEvidence::default(),
        );
        drop(tx);
        assert_eq!(rx.recv().unwrap().0.len(), 1);
        assert!(matches!(rx.recv(), Err(RecvError)));
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(1)),
            Err(RecvTimeoutError::Disconnected)
        ));
    }

    #[test]
    fn slow_observer_gets_explicit_execution_evidence_overflow() {
        let (tx, rx) = rows_channel();
        tx.send_evidence(
            (1..=300)
                .map(|sequence| ObservationEvidence {
                    sequence,
                    fact: ObservationFact::Withdrawn,
                })
                .collect(),
        );

        let (_, _, execution) = rx.recv().unwrap();
        assert_eq!(execution.len(), EXECUTION_EVIDENCE_CAPACITY);
        assert!(matches!(
            &execution[0],
            ObservationEvidence {
                sequence: 45,
                fact: ObservationFact::Overflow {
                    first_sequence: 1,
                    last_sequence: 45,
                    dropped: 45,
                },
            }
        ));
        assert_eq!(execution[1].sequence, 46);
        assert_eq!(execution.last().unwrap().sequence, 300);
    }

    #[test]
    fn execution_only_batch_preserves_latest_acquisition_evidence() {
        let (tx, rx) = rows_channel();
        let evidence = latest_evidence();
        send_rows(&tx, Vec::new(), evidence.clone());
        assert_eq!(rx.recv().unwrap().1, evidence);

        tx.send_evidence(vec![ObservationEvidence {
            sequence: 1,
            fact: ObservationFact::Withdrawn,
        }]);

        let (deltas, received_evidence, execution) = rx.recv().unwrap();
        assert!(deltas.is_empty());
        assert_eq!(received_evidence, evidence);
        assert_eq!(execution.len(), 1);
    }
}
