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
    /// Present at the receiver's baseline and now, with the latest complete
    /// row value (currently signature promotion).
    Updated(Row),
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
    evidence: Vec<AcquisitionEvidence>,
    execution: VecDeque<ObservationEvidence>,
}

const EXECUTION_EVIDENCE_CAPACITY: usize = 256;

impl PendingRows {
    fn new(evidence: Vec<AcquisitionEvidence>) -> Self {
        Self {
            by_id: BTreeMap::new(),
            evidence,
            execution: VecDeque::new(),
        }
    }

    fn push_execution(&mut self, facts: Vec<ObservationEvidence>) {
        push_execution_capped(&mut self.execution, facts);
    }

    fn push(&mut self, delta: RowDelta) {
        let id = delta.id();
        let previous = self.by_id.remove(&id);
        let next = match (previous, delta) {
            (None, RowDelta::Added(row)) => Some(PendingTransition::Added(row)),
            (None, RowDelta::Updated(row)) => Some(PendingTransition::Updated(row)),
            (None, RowDelta::SourcesGrew { sources, .. }) => {
                Some(PendingTransition::SourcesGrew(sources))
            }
            (None, RowDelta::Removed(_)) => Some(PendingTransition::Removed),

            (Some(PendingTransition::Added(_)), RowDelta::Added(row)) => {
                Some(PendingTransition::Added(row))
            }
            (Some(PendingTransition::Added(_)), RowDelta::Updated(row)) => {
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
            (Some(PendingTransition::SourcesGrew(_)), RowDelta::Updated(row)) => {
                Some(PendingTransition::Updated(row))
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
            (Some(PendingTransition::Removed), RowDelta::Updated(_)) => {
                Some(PendingTransition::Removed)
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
            (Some(PendingTransition::Replaced(_)), RowDelta::Updated(row)) => {
                Some(PendingTransition::Replaced(row))
            }
            (Some(PendingTransition::Replaced(mut row)), RowDelta::SourcesGrew { sources, .. }) => {
                row.sources = sources;
                Some(PendingTransition::Replaced(row))
            }
            (Some(PendingTransition::Replaced(_)), RowDelta::Removed(_)) => {
                Some(PendingTransition::Removed)
            }

            (Some(PendingTransition::Updated(_)), RowDelta::Added(row))
            | (Some(PendingTransition::Updated(_)), RowDelta::Updated(row)) => {
                Some(PendingTransition::Updated(row))
            }
            (Some(PendingTransition::Updated(mut row)), RowDelta::SourcesGrew { sources, .. }) => {
                row.sources = sources;
                Some(PendingTransition::Updated(row))
            }
            (Some(PendingTransition::Updated(_)), RowDelta::Removed(_)) => {
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
                PendingTransition::Updated(row) => deltas.push(RowDelta::Updated(row)),
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

/// What this mailbox may say about acquisition, as the two states that are
/// actually distinguishable — never one `Vec` doing both jobs (#1276).
///
/// The distinction is load-bearing twice over. `AcquisitionEvidence` is
/// per-CANONICAL-BRANCH and positional: `RowBatch.evidence[i]` is the fact
/// about `branches[i]` on every SDK, so a frame carrying a shorter vector
/// breaks an index correspondence rather than merely reporting less. And
/// evidence describes the state the delivered ROWS came from, so a frame may
/// never carry evidence for a projection whose rows the consumer has not been
/// given — `nmp-nip02` reads `availability` off `frame.evidence` and the
/// contact list off the deltas it has folded, and pairing a proven source with
/// rows that have not arrived makes it report `NoContactList` for an account
/// that has one.
enum Acquisition {
    /// The opening row frame has not reached this mailbox yet. Execution facts
    /// issued in the meantime wait HERE rather than becoming a frame of their
    /// own: there is no honest evidence to give such a frame. They ride out on
    /// the opening frame, which always follows.
    BeforeOpening(VecDeque<ObservationEvidence>),
    /// Per-branch evidence as of the last delivered row frame. Execution-only
    /// facts compose onto this, which is why it is retained at all — otherwise
    /// they would replace real evidence with `AcquisitionEvidence::default()`.
    Delivered(Vec<AcquisitionEvidence>),
}

pub(crate) struct RowsSender {
    pending: LatestSender<PendingRows>,
    acquisition: Mutex<Acquisition>,
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
/// use nmp::mechanism::runtime::RowsReceiver;
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
            acquisition: Mutex::new(Acquisition::BeforeOpening(VecDeque::new())),
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
        // Whatever was issued before the opening frame rides out ON it, ahead
        // of this frame's own facts: one ordered trace, nothing dropped, and
        // no frame delivered before the projection it reports on.
        let mut carried = {
            let mut acquisition = self.acquisition.lock().unwrap();
            match std::mem::replace(&mut *acquisition, Acquisition::Delivered(evidence.clone())) {
                Acquisition::BeforeOpening(waiting) => waiting,
                Acquisition::Delivered(_) => VecDeque::new(),
            }
        };
        push_execution_capped(&mut carried, execution);
        self.pending.update(|pending| {
            let pending = pending.get_or_insert_with(|| PendingRows::new(evidence.clone()));
            for delta in deltas {
                pending.push(delta);
            }
            pending.evidence = evidence;
            pending.push_execution(carried.iter().cloned().collect());
        });
        #[cfg(feature = "bench-instrumentation")]
        crate::ingest_attribution::row_channel_send(send_started.elapsed(), delta_count);
    }

    pub(crate) fn send_evidence(&self, execution: Vec<ObservationEvidence>) {
        let evidence = {
            let mut acquisition = self.acquisition.lock().unwrap();
            match &mut *acquisition {
                // No opening frame yet, so there is no evidence a frame could
                // honestly carry. Hold the facts for the frame that will.
                Acquisition::BeforeOpening(waiting) => {
                    push_execution_capped(waiting, execution);
                    return;
                }
                Acquisition::Delivered(evidence) => evidence.clone(),
            }
        };
        self.pending.update(|pending| {
            let pending = pending.get_or_insert_with(|| PendingRows::new(evidence));
            pending.push_execution(execution);
        });
    }
}

/// Append `facts`, collapsing the oldest into ONE `Overflow` fact once the
/// queue would exceed [`EXECUTION_EVIDENCE_CAPACITY`]. Shared by the pending
/// frame and by the pre-opening hold, so a slow opening cannot grow unbounded
/// where a slow consumer could not.
fn push_execution_capped(
    queue: &mut VecDeque<ObservationEvidence>,
    facts: Vec<ObservationEvidence>,
) {
    queue.extend(facts);
    if queue.len() <= EXECUTION_EVIDENCE_CAPACITY {
        return;
    }

    let mut first = u64::MAX;
    let mut last = 0;
    let mut dropped = 0u64;
    while queue
        .front()
        .is_some_and(|fact| matches!(fact.fact, ObservationFact::Overflow { .. }))
    {
        let prior = queue.pop_front().expect("front existed");
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
    while queue.len() >= EXECUTION_EVIDENCE_CAPACITY {
        let removed = queue.pop_front().expect("length checked");
        first = first.min(removed.sequence);
        last = last.max(removed.sequence);
        dropped = dropped.saturating_add(1);
    }
    queue.push_front(ObservationEvidence {
        branch: None,
        sequence: last,
        fact: ObservationFact::Overflow {
            first_sequence: first,
            last_sequence: last,
            dropped,
        },
    });
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

    use nmp_grammar::ConcreteFilter;

    use super::*;
    use crate::core::ShortfallFact;

    fn row(keys: &Keys, created_at: u64, content: &str) -> Row {
        Row::from_relay_event(
            UnsignedEvent::new(
                keys.public_key(),
                Timestamp::from(created_at),
                Kind::TextNote,
                Vec::new(),
                content,
            )
            .sign_with_keys(keys)
            .unwrap(),
            BTreeSet::new(),
        )
    }

    fn apply(rows: &mut BTreeMap<EventId, Row>, deltas: &[RowDelta]) {
        for delta in deltas {
            match delta {
                RowDelta::Added(row) => {
                    rows.insert(row.id(), row.clone());
                }
                RowDelta::Updated(row) => {
                    rows.insert(row.id(), row.clone());
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

    /// One observation's opening per-branch acquisition evidence, in the exact
    /// shape `open_observation` computes: one entry per canonical branch, in
    /// branch order, each naming its OWN branch so a collapse or a reorder is
    /// visible rather than absorbed by a length check.
    fn opening_evidence(branches: usize) -> Vec<AcquisitionEvidence> {
        (0..branches)
            .map(|branch| AcquisitionEvidence {
                sources: Vec::new(),
                shortfall: vec![ShortfallFact::NoPlannedSource {
                    atom: ConcreteFilter {
                        kinds: Some(BTreeSet::from([branch as u16])),
                        ..ConcreteFilter::default()
                    },
                }],
            })
            .collect()
    }

    fn latest_evidence() -> Vec<AcquisitionEvidence> {
        vec![AcquisitionEvidence {
            sources: Vec::new(),
            shortfall: vec![ShortfallFact::NoResolvedDemand],
        }]
    }

    fn send_rows(tx: &RowsSender, deltas: Vec<RowDelta>, evidence: Vec<AcquisitionEvidence>) {
        tx.send((deltas, evidence, Vec::new()));
    }

    #[test]
    fn ten_thousand_skipped_updates_form_one_exact_transition() {
        fn assert_send<T: Send>() {}
        assert_send::<RowsReceiver>();

        let keys = Keys::generate();
        let mut expected = row(&keys, 1, "same-event");
        let id = expected.id();
        let (tx, rx) = rows_channel();
        send_rows(
            &tx,
            vec![RowDelta::Added(expected.clone())],
            vec![AcquisitionEvidence::default()],
        );
        let mut delivered = BTreeMap::new();
        apply(&mut delivered, &rx.recv().unwrap().0);

        for update in 0..5_000 {
            send_rows(
                &tx,
                vec![RowDelta::Removed(id)],
                vec![AcquisitionEvidence::default()],
            );
            expected.sources = [RelayUrl::parse(&format!("wss://r{update}.example")).unwrap()]
                .into_iter()
                .collect();
            send_rows(
                &tx,
                vec![RowDelta::Added(expected.clone())],
                vec![AcquisitionEvidence::default()],
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
            vec![AcquisitionEvidence::default()],
        );
        let evidence = latest_evidence();
        send_rows(&tx, vec![RowDelta::Removed(added.id())], evidence.clone());

        let (deltas, received_evidence, _) = rx.recv().unwrap();
        assert!(deltas.is_empty());
        assert_eq!(received_evidence, evidence);
    }

    #[test]
    fn source_growth_keeps_only_the_latest_complete_source_set() {
        let keys = Keys::generate();
        let initial = row(&keys, 1, "provenance");
        let id = initial.id();
        let a = RelayUrl::parse("wss://a.example").unwrap();
        let b = RelayUrl::parse("wss://b.example").unwrap();
        let (tx, rx) = rows_channel();
        send_rows(
            &tx,
            vec![RowDelta::Added(initial)],
            vec![AcquisitionEvidence::default()],
        );
        rx.recv().unwrap();
        send_rows(
            &tx,
            vec![RowDelta::SourcesGrew {
                id,
                sources: [a.clone()].into_iter().collect(),
            }],
            vec![AcquisitionEvidence::default()],
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
    fn slow_observer_never_retains_a_pending_row_after_signature_promotion() {
        let keys = Keys::generate();
        let signed = row(&keys, 1, "promoted while observer is slow");
        let pending = Row::from_stored_event(
            {
                let mut event = signed.event_for_store();
                event.sig = nmp_store::sentinel_signature();
                event
            },
            nmp_store::SigState::Pending,
            signed.sources.clone(),
        );

        let (tx, rx) = rows_channel();
        send_rows(
            &tx,
            vec![RowDelta::Added(pending)],
            vec![AcquisitionEvidence::default()],
        );
        // The receiver has not consumed the optimistic frame. Promotion must
        // compose into the pending Added and replace its complete payload.
        send_rows(
            &tx,
            vec![RowDelta::Updated(signed.clone())],
            latest_evidence(),
        );

        let (deltas, evidence, _) = rx.recv().unwrap();
        assert!(matches!(
            deltas.as_slice(),
            [RowDelta::Added(row)]
                if row == &signed
                    && matches!(row.signature, crate::core::RowSignature::Signed(_))
                    && row.signed_event().is_some_and(|event| event.verify().is_ok())
        ));
        assert_eq!(evidence, latest_evidence());
    }

    #[test]
    fn source_growth_after_removal_fails_closed_without_resurrecting_the_row() {
        let keys = Keys::generate();
        let initial = row(&keys, 1, "must-stay-removed");
        let id = initial.id();
        let (tx, rx) = rows_channel();
        send_rows(
            &tx,
            vec![RowDelta::Added(initial)],
            vec![AcquisitionEvidence::default()],
        );
        let mut delivered = BTreeMap::new();
        apply(&mut delivered, &rx.recv().unwrap().0);

        send_rows(
            &tx,
            vec![RowDelta::Removed(id)],
            vec![AcquisitionEvidence::default()],
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
            vec![AcquisitionEvidence::default()],
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
        // Open first: execution-only frames exist only once the observation's
        // opening projection has been delivered (#1276).
        send_rows(&tx, Vec::new(), opening_evidence(1));
        rx.recv().unwrap();
        tx.send_evidence(
            (1..=300)
                .map(|sequence| ObservationEvidence {
                    branch: Some(0),
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
                branch: None,
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
            branch: Some(0),
            sequence: 1,
            fact: ObservationFact::Withdrawn,
        }]);

        let (deltas, received_evidence, execution) = rx.recv().unwrap();
        assert!(deltas.is_empty());
        assert_eq!(received_evidence, evidence);
        assert_eq!(execution.len(), 1);
    }

    /// #1276: an observation's opening execution facts reach this mailbox
    /// BEFORE its opening row emit does — `Cmd::Subscribe` hands the receiver
    /// back before it dispatches, and each branch's `ConcreteFilter` fact is
    /// dispatched ahead of the opening `Effect::EmitRows`. They must not become
    /// a frame of their own: the OPENING frame is the first frame, carrying
    /// those facts, its rows, and one evidence entry per canonical branch.
    ///
    /// Two disablements must turn this red, and they are the two ways the
    /// mailbox can lie:
    ///
    /// - Let `send_evidence` create a frame from an empty acquisition snapshot
    ///   (the shipped defect). The first frame then reports evidence for ZERO
    ///   branches, so `RowBatch.evidence[i]` stops being the fact about
    ///   `branches[i]` on both SDKs.
    /// - Let it create a frame from the opening projection's evidence instead.
    ///   The count is right, but the frame now reports a proven source while
    ///   carrying none of the rows that proof came from — which is what makes
    ///   `nmp-nip02` report `NoContactList` for an account that has one, caught
    ///   by `nmp-parity`'s `direct_and_ffi_follow_actions_are_identical_over_real_loopback`.
    #[test]
    fn opening_execution_facts_ride_out_on_the_opening_frame() {
        let opening = opening_evidence(2);
        let keys = Keys::generate();
        let row = row(&keys, 1, "opening row");
        let (tx, rx) = rows_channel();

        tx.send_evidence(vec![ObservationEvidence {
            branch: Some(1),
            sequence: 1,
            fact: ObservationFact::Withdrawn,
        }]);
        assert!(
            matches!(rx.try_recv(), Err(TryRecvError::Empty)),
            "no frame precedes the opening projection"
        );

        tx.send((
            vec![RowDelta::Added(row.clone())],
            opening.clone(),
            Vec::new(),
        ));

        let (deltas, evidence, execution) = rx.recv().unwrap();
        assert!(
            matches!(deltas.as_slice(), [RowDelta::Added(delivered)] if delivered == &row),
            "the opening frame carries the opening rows"
        );
        assert_eq!(
            evidence, opening,
            "one evidence entry per canonical branch, for the rows just delivered"
        );
        assert_eq!(
            execution.len(),
            1,
            "the fact issued before the opening frame is carried BY it, never dropped"
        );
        assert_eq!(execution[0].sequence, 1);
    }

    /// The pre-opening hold is bounded exactly like the pending frame's own
    /// execution queue: a slow opening cannot grow memory where a slow consumer
    /// could not, and the collapse is reported rather than silent.
    #[test]
    fn facts_held_for_a_slow_opening_overflow_explicitly() {
        let (tx, rx) = rows_channel();
        tx.send_evidence(
            (1..=300)
                .map(|sequence| ObservationEvidence {
                    branch: Some(0),
                    sequence,
                    fact: ObservationFact::Withdrawn,
                })
                .collect(),
        );

        tx.send((Vec::new(), opening_evidence(1), Vec::new()));

        let (_, _, execution) = rx.recv().unwrap();
        assert_eq!(execution.len(), EXECUTION_EVIDENCE_CAPACITY);
        assert!(matches!(
            &execution[0],
            ObservationEvidence {
                sequence: 45,
                branch: None,
                fact: ObservationFact::Overflow {
                    first_sequence: 1,
                    last_sequence: 45,
                    dropped: 45,
                },
            }
        ));
        assert_eq!(execution.last().unwrap().sequence, 300);
    }
}
