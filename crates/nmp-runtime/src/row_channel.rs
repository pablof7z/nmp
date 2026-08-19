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
use std::collections::{BTreeMap, BTreeSet};
use std::marker::PhantomData;
use std::sync::mpsc::{RecvError, RecvTimeoutError, TryRecvError};
use std::time::Duration;

use nostr::{EventId, RelayUrl};

use nmp_engine::core::{AcquisitionEvidence, Row, RowDelta};

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
    /// Per-CANONICAL-BRANCH and positional: `RowBatch.evidence[i]` is the
    /// fact about `branches[i]`, so a frame carrying a shorter vector breaks
    /// an index correspondence rather than merely reporting less. Evidence
    /// also describes the state the delivered ROWS came from, so a frame may
    /// never carry evidence for a projection whose rows the consumer has not
    /// been given -- `nmp-nip02` reads `availability` off `frame.evidence`
    /// and the contact list off the deltas it has folded, and pairing a
    /// proven source with rows that have not arrived makes it report
    /// `NoContactList` for an account that has one (#1276).
    evidence: Vec<AcquisitionEvidence>,
}

impl PendingRows {
    fn new(evidence: Vec<AcquisitionEvidence>) -> Self {
        Self {
            by_id: BTreeMap::new(),
            evidence,
        }
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
        (deltas, self.evidence)
    }
}

pub(crate) struct RowsSender {
    pending: LatestSender<PendingRows>,
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
        RowsSender { pending: sender },
        RowsReceiver {
            pending: receiver,
            not_sync: PhantomData,
        },
    )
}

impl RowsSender {
    pub(crate) fn send(&self, (deltas, evidence): RowsMsg) {
        #[cfg(feature = "bench-instrumentation")]
        let send_started = std::time::Instant::now();
        #[cfg(feature = "bench-instrumentation")]
        let delta_count = deltas.len();
        self.pending.update(|pending| {
            let pending = pending.get_or_insert_with(|| PendingRows::new(evidence.clone()));
            for delta in deltas {
                pending.push(delta);
            }
            pending.evidence = evidence;
        });
        #[cfg(feature = "bench-instrumentation")]
        nmp_engine::ingest_attribution::row_channel_send(send_started.elapsed(), delta_count);
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

