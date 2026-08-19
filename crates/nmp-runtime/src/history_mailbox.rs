use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::mpsc::{RecvError, RecvTimeoutError};
use std::sync::Mutex;
use std::time::Duration;

use nostr::EventId;

use super::diagnostics_channel::{AsyncLatestReceiver, ConcurrentNext, LatestReceiver};
use nmp_engine::core::{HistoryBatch, Row, RowDelta};

pub type HistoryMsg = HistoryBatch;

/// Receiver for one bounded, latest-wins history stream.
///
/// The single-slot mailbox stores a complete current frame. On receipt we
/// derive `deltas` against this receiver's last delivered frame, rather than
/// trusting the reducer-adjacent delta that may span an overwritten frame.
/// Both retained maps are bounded by the session's declared `max_rows`.
/// Like `std::sync::mpsc::Receiver`, this is a single-consumer value: it is
/// `Send` but deliberately not `Sync`.
///
/// ```compile_fail
/// use nmp::mechanism::runtime::HistoryReceiver;
/// fn require_sync<T: Sync>() {}
/// require_sync::<HistoryReceiver>();
/// ```
pub struct HistoryReceiver {
    batches: LatestReceiver<HistoryBatch>,
    pub(super) delivered: RefCell<BTreeMap<EventId, Row>>,
}

impl HistoryReceiver {
    pub(super) fn new(batches: LatestReceiver<HistoryBatch>) -> Self {
        Self {
            batches,
            delivered: RefCell::new(BTreeMap::new()),
        }
    }

    pub fn recv(&self) -> Result<HistoryBatch, RecvError> {
        let batch = self.batches.recv().ok_or(RecvError)?;
        let mut delivered = self.delivered.borrow_mut();
        Ok(Self::reconcile(&mut delivered, batch))
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Result<HistoryBatch, RecvTimeoutError> {
        let batch = self.batches.recv_timeout(timeout)?;
        let mut delivered = self.delivered.borrow_mut();
        Ok(Self::reconcile(&mut delivered, batch))
    }

    /// Convert to the `Send + Sync` async pull surface (#680). The
    /// receiver-side `delivered` reconcile map moves behind a `Mutex`; the
    /// single-reader guard on the async receiver means `next()` never contends
    /// it with itself.
    pub fn into_async(self) -> AsyncHistoryReceiver {
        AsyncHistoryReceiver {
            batches: AsyncLatestReceiver::new(self.batches),
            delivered: Mutex::new(self.delivered.into_inner()),
        }
    }

    fn reconcile(delivered: &mut BTreeMap<EventId, Row>, mut batch: HistoryBatch) -> HistoryBatch {
        let current: BTreeMap<_, _> = batch
            .rows
            .iter()
            .cloned()
            .map(|row| (row.id(), row))
            .collect();
        debug_assert_eq!(current.len(), batch.rows.len());

        let mut deltas = Vec::new();
        for row in &batch.rows {
            match delivered.get(&row.id()) {
                None => deltas.push(RowDelta::Added(row.clone())),
                Some(previous) if previous.sources != row.sources => {
                    deltas.push(RowDelta::SourcesGrew {
                        id: row.id(),
                        sources: row.sources.clone(),
                    });
                }
                Some(_) => {}
            }
        }
        for event_id in delivered.keys() {
            if !current.contains_key(event_id) {
                deltas.push(RowDelta::Removed(*event_id));
            }
        }
        *delivered = current;
        batch.deltas = deltas;
        batch
    }
}

/// The async single-consumer half of a bounded, latest-wins history stream
/// (#680). `Send + Sync`: the receiver-side reconcile map is behind a `Mutex`,
/// and the single-reader guard on [`AsyncLatestReceiver`] serialises `next()`.
pub struct AsyncHistoryReceiver {
    batches: AsyncLatestReceiver<HistoryBatch>,
    delivered: Mutex<BTreeMap<EventId, Row>>,
}

impl AsyncHistoryReceiver {
    /// Await the next bounded latest snapshot with exact deltas rebased against
    /// this receiver's last delivered frame, or `None` once the producer is
    /// gone / the consumer cancelled. [`ConcurrentNext`] on an overlapping call.
    pub async fn next(&self) -> Result<Option<HistoryBatch>, ConcurrentNext> {
        match self.batches.next().await? {
            Some(batch) => {
                let mut delivered = self.delivered.lock().unwrap();
                Ok(Some(HistoryReceiver::reconcile(&mut delivered, batch)))
            }
            None => Ok(None),
        }
    }

    /// Idempotent consumer-initiated close; wakes a parked `next()` to `None`.
    pub fn close(&self) {
        self.batches.close();
    }
}
