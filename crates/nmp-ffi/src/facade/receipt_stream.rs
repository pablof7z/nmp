use std::sync::{Arc, Mutex};

#[cfg(feature = "nip65")]
use super::AutomaticRoutingAssembly;
use super::NmpEngine;
use crate::convert::{
    cancel_write_error_to_ffi, cancel_write_outcome_to_ffi, parse_event_id,
    publish_queue_entry_to_ffi, publish_queue_error_to_ffi, receipt_result_to_ffi,
    remove_queue_entry_error_to_ffi, write_intent_from_ffi, write_status_to_ffi, FfiError,
    WriteStatusRef,
};
use crate::types::{
    FfiCancelWriteError, FfiCancelWriteOutcome, FfiCorrelationReattachment, FfiPublishQueueEntry,
    FfiPublishQueueError, FfiReceiptReattachment, FfiReceiptResult, FfiRemoveQueueEntryError,
    FfiWriteFact, FfiWriteIntent,
};
use nmp::ReceiptReattachment;

/// The app-facing pull-based receipt stream (returned by [`NmpEngine::publish`]
/// and the `Attached` reattachment, #680). It
/// exposes the stable store-issued receipt id via [`Self::id`] and delivers
/// ordered `WriteFact` facts via `async fn next()`. Live delivery is a finite
/// FIFO that reports typed lag. Receipt facts are durable: the persisted
/// publish-queue Redb store is the source of truth, so a dropped or lagged stream can
/// be reattached and traverse retained facts through finite pages.
#[derive(uniffi::Object)]
pub struct NmpReceiptStream {
    id: nmp::ReceiptId,
    engine: Option<Arc<nmp::Engine>>,
    delivery: Mutex<ReceiptDelivery>,
    // Concurrency guard only, never lifecycle/ownership state: cancellation
    // lives in `ReceiptDelivery`, and this flag is released by the RAII
    // `ReceiptReadingGuard` on success, error, or future drop (gate 3).
    reading: std::sync::atomic::AtomicBool,
}

enum ReceiptDelivery {
    Active {
        receiver: Arc<nmp::AsyncFifoReceiver<nmp::WriteFact>>,
        next_cursor: Option<nmp::ReceiptReplayCursor>,
    },
    Cancelled,
}

#[uniffi::export]
impl NmpEngine {
    /// Enqueue a write (#680). The returned [`NmpReceiptStream`] exposes the
    /// stable receipt id ([`NmpReceiptStream::id`]) and streams every
    /// `WriteFact` this intent ever reaches (ledger #9 -- enqueue is not
    /// converged; the first value is never a terminal for a durable/
    /// at-most-once intent) via `async fn next()`. A caller-supplied `Signed`
    /// payload that fails verification is no longer a synchronous error here
    /// (that guarantee moved to `nmp-engine::core::EngineCore::on_publish`'s
    /// acceptance boundary, Unit A0/#56, so it holds for every entry point) --
    /// it refuses THIS CALL as `FfiError::PublishRefused`, taking nothing
    /// into custody, so no receipt, no stream and no queue entry exist for
    /// it. Exhaustion of the pre-acceptance correlation namespace is the
    /// same shape: a typed `FfiError` and no receipt id.
    pub fn publish(&self, intent: FfiWriteIntent) -> Result<Arc<NmpReceiptStream>, FfiError> {
        let write_intent = write_intent_from_ffi(intent)?;
        #[cfg(feature = "nip65")]
        if matches!(write_intent.routing, nmp::WriteRouting::Auto)
            && self.automatic_routing == AutomaticRoutingAssembly::Unavailable
        {
            return Err(FfiError::AutomaticRoutingUnavailable);
        }
        let receipt = self.engine.publish(write_intent)?;
        Ok(NmpReceiptStream::new(self.engine.clone(), receipt))
    }

    /// Attach to a retained receipt without collapsing corrupt durable
    /// evidence into the same result as an unknown id (#680). The `Attached`
    /// variant carries an [`NmpReceiptStream`] that transparently traverses
    /// durable `WriteFact` facts in finite pages and streams onward,
    /// delivered pull-based via `async fn next()`.
    pub fn reattach_receipt(&self, receipt_id: u64) -> Result<FfiReceiptReattachment, FfiError> {
        let result = self.engine.reattach_receipt(nmp::ReceiptId(receipt_id))?;
        Ok(match result {
            ReceiptReattachment::Attached {
                id,
                statuses,
                next_cursor,
            } => FfiReceiptReattachment::Attached {
                stream: NmpReceiptStream::from_reattachment(
                    self.engine.clone(),
                    id,
                    statuses,
                    next_cursor,
                ),
            },
            ReceiptReattachment::NotFound => FfiReceiptReattachment::NotFound,
            ReceiptReattachment::RetainedButUnreadable => {
                FfiReceiptReattachment::RetainedButUnreadable
            }
        })
    }

    /// #591: recover a receipt after a crash that happened BEFORE the app
    /// could durably persist the receipt id `publish`
    /// returned -- looked up by the caller's own crash-safe correlation
    /// token instead. Otherwise identical to [`Self::reattach_receipt`],
    /// except the caller cannot already know the receipt id (that is
    /// exactly what a token recovers) -- `FfiCorrelationReattachment.
    /// receipt_id` carries it back, `Some` iff `outcome == Attached`.
    pub fn reattach_by_correlation(
        &self,
        correlation: String,
    ) -> Result<FfiCorrelationReattachment, FfiError> {
        let result = self.engine.reattach_by_correlation(correlation)?;
        let receipt_id = match &result {
            ReceiptReattachment::Attached { id, .. } => Some(id.0),
            ReceiptReattachment::NotFound | ReceiptReattachment::RetainedButUnreadable => None,
        };
        let outcome = match result {
            ReceiptReattachment::Attached {
                id,
                statuses,
                next_cursor,
            } => FfiReceiptReattachment::Attached {
                stream: NmpReceiptStream::from_reattachment(
                    self.engine.clone(),
                    id,
                    statuses,
                    next_cursor,
                ),
            },
            ReceiptReattachment::NotFound => FfiReceiptReattachment::NotFound,
            ReceiptReattachment::RetainedButUnreadable => {
                FfiReceiptReattachment::RetainedButUnreadable
            }
        };
        Ok(FfiCorrelationReattachment {
            outcome,
            receipt_id,
        })
    }

    /// Read the app's own publish queue back (#1039).
    ///
    /// INSPECTION, never waiting: this returns what NMP knows right now and
    /// never blocks on settlement.
    pub fn publish_queue(
        &self,
        after_receipt_id: Option<u64>,
        limit: u8,
    ) -> Result<Vec<FfiPublishQueueEntry>, FfiPublishQueueError> {
        self.engine
            .publish_queue(after_receipt_id.map(nmp::ReceiptId), limit)
            .map(|entries| entries.iter().map(publish_queue_entry_to_ffi).collect())
            .map_err(publish_queue_error_to_ffi)
    }

    /// Read one bounded page of currently open obligations for the exact
    /// event id carried by a query row (#903).
    pub fn publish_queue_for_event(
        &self,
        event_id: String,
        after_receipt_id: Option<u64>,
        limit: u8,
    ) -> Result<Vec<FfiPublishQueueEntry>, FfiPublishQueueError> {
        let event_id =
            parse_event_id(&event_id).map_err(|error| FfiPublishQueueError::InvalidEventId {
                reason: error.to_string(),
            })?;
        self.engine
            .publish_queue_for_event(event_id, after_receipt_id.map(nmp::ReceiptId), limit)
            .map(|entries| entries.iter().map(publish_queue_entry_to_ffi).collect())
            .map_err(publish_queue_error_to_ffi)
    }

    /// Forget one queue entry (#1039). How a write parked forever on a
    /// missing signer, or a permanently-failed refused entry, ever ends —
    /// the parked one through `cancel_write` first, which ends the obligation
    /// and compensates the optimistic row, leaving the terminal receipt this
    /// door then forgets. An entry whose obligation is still open is refused.
    pub fn remove_publish_queue_entry(
        &self,
        receipt_id: u64,
    ) -> Result<(), FfiRemoveQueueEntryError> {
        self.engine
            .remove_publish_queue_entry(nmp::ReceiptId(receipt_id))
            .map_err(remove_queue_entry_error_to_ffi)
    }

    /// Explicitly cancel one accepted unsigned write. A successful outcome
    /// means the matching durable terminal fact was delivered to receipt
    /// observers.
    pub fn cancel(&self, receipt_id: u64) -> Result<FfiCancelWriteOutcome, FfiCancelWriteError> {
        self.engine
            .cancel(nmp::ReceiptId(receipt_id))
            .map(cancel_write_outcome_to_ffi)
            .map_err(cancel_write_error_to_ffi)
    }
}

impl NmpReceiptStream {
    pub(crate) fn new(engine: Arc<nmp::Engine>, receipt: nmp::ReceiptStream) -> Arc<Self> {
        Arc::new(Self {
            id: receipt.id,
            engine: Some(engine),
            delivery: Mutex::new(ReceiptDelivery::Active {
                receiver: Arc::new(receipt.statuses.into_async()),
                next_cursor: None,
            }),
            reading: std::sync::atomic::AtomicBool::new(false),
        })
    }

    pub(super) fn from_reattachment(
        engine: Arc<nmp::Engine>,
        id: nmp::ReceiptId,
        statuses: nmp::FifoReceiver<nmp::WriteFact>,
        next_cursor: Option<nmp::ReceiptReplayCursor>,
    ) -> Arc<Self> {
        Arc::new(Self {
            id,
            engine: Some(engine),
            delivery: Mutex::new(ReceiptDelivery::Active {
                receiver: Arc::new(statuses.into_async()),
                next_cursor,
            }),
            reading: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn current_receiver(
        &self,
    ) -> Option<(
        Arc<nmp::AsyncFifoReceiver<nmp::WriteFact>>,
        Option<nmp::ReceiptReplayCursor>,
    )> {
        let delivery = self.delivery.lock().unwrap();
        match &*delivery {
            ReceiptDelivery::Active {
                receiver,
                next_cursor,
            } => Some((receiver.clone(), next_cursor.clone())),
            ReceiptDelivery::Cancelled => None,
        }
    }

    fn install_page(
        &self,
        prior: &Arc<nmp::AsyncFifoReceiver<nmp::WriteFact>>,
        statuses: nmp::FifoReceiver<nmp::WriteFact>,
        next_cursor: Option<nmp::ReceiptReplayCursor>,
    ) -> bool {
        let replacement = Arc::new(statuses.into_async());
        let mut delivery = self.delivery.lock().unwrap();
        match &mut *delivery {
            ReceiptDelivery::Active {
                receiver,
                next_cursor: cursor,
            } if Arc::ptr_eq(receiver, prior) => {
                *receiver = replacement;
                *cursor = next_cursor;
                true
            }
            ReceiptDelivery::Active { .. } | ReceiptDelivery::Cancelled => {
                replacement.close();
                false
            }
        }
    }

    pub(super) fn replace_page(
        &self,
        statuses: nmp::FifoReceiver<nmp::WriteFact>,
        next_cursor: Option<nmp::ReceiptReplayCursor>,
    ) -> bool {
        let replacement = Arc::new(statuses.into_async());
        let mut delivery = self.delivery.lock().unwrap();
        match &*delivery {
            ReceiptDelivery::Active { .. } => {
                *delivery = ReceiptDelivery::Active {
                    receiver: replacement,
                    next_cursor,
                };
                true
            }
            ReceiptDelivery::Cancelled => {
                replacement.close();
                false
            }
        }
    }

    async fn next_fact(&self) -> Result<Option<nmp::WriteFact>, FfiError> {
        loop {
            let Some((receiver, next_cursor)) = self.current_receiver() else {
                return Ok(None);
            };
            match receiver.next().await {
                Ok(Some(status)) => return Ok(Some(status)),
                Err(nmp::FifoNextError::ConcurrentNext) => return Err(FfiError::ConcurrentNext),
                Err(nmp::FifoNextError::Lagged) => {
                    return Err(FfiError::FactStreamLagged {
                        receipt_id: Some(self.id.0),
                    });
                }
                Ok(None) => {}
            }

            let Some(cursor) = next_cursor else {
                return Ok(None);
            };
            let Some(engine) = &self.engine else {
                return Err(FfiError::FactStreamLagged {
                    receipt_id: Some(self.id.0),
                });
            };
            match engine.reattach_receipt_from(self.id, cursor)? {
                ReceiptReattachment::Attached {
                    id,
                    statuses,
                    next_cursor,
                } if id == self.id => {
                    if !self.install_page(&receiver, statuses, next_cursor) {
                        return Ok(None);
                    }
                }
                ReceiptReattachment::Attached { .. }
                | ReceiptReattachment::NotFound
                | ReceiptReattachment::RetainedButUnreadable => {
                    return Err(FfiError::ReceiptReplayUnavailable {
                        receipt_id: self.id.0,
                    });
                }
            }
        }
    }

    fn restart_replay(&self) -> Result<(), FfiError> {
        let Some(engine) = &self.engine else {
            return Err(FfiError::ReceiptReplayUnavailable {
                receipt_id: self.id.0,
            });
        };
        match engine.reattach_receipt(self.id)? {
            ReceiptReattachment::Attached {
                id,
                statuses,
                next_cursor,
            } if id == self.id => {
                if self.replace_page(statuses, next_cursor) {
                    Ok(())
                } else {
                    Err(FfiError::ReceiptReplayUnavailable {
                        receipt_id: self.id.0,
                    })
                }
            }
            ReceiptReattachment::Attached { .. }
            | ReceiptReattachment::NotFound
            | ReceiptReattachment::RetainedButUnreadable => {
                Err(FfiError::ReceiptReplayUnavailable {
                    receipt_id: self.id.0,
                })
            }
        }
    }
}

#[uniffi::export]
impl NmpReceiptStream {
    /// The stable store-issued receipt id, needed for process-later
    /// reattachment ([`NmpEngine::reattach_receipt`]) and explicit cancellation
    /// ([`NmpEngine::cancel`]).
    pub fn id(&self) -> u64 {
        self.id.0
    }

    /// Await the next `WriteFact`, or `None` once the intent has fully
    /// resolved or the engine has shut down. [`FfiError::ConcurrentNext`] on an
    /// overlapping call.
    pub async fn next(&self) -> Result<Option<FfiWriteFact>, FfiError> {
        use std::sync::atomic::Ordering;

        if self.reading.swap(true, Ordering::AcqRel) {
            return Err(FfiError::ConcurrentNext);
        }
        let _reading = ReceiptReadingGuard(&self.reading);
        Ok(self
            .next_fact()
            .await?
            .map(|status| write_status_to_ffi(WriteStatusRef(&status))))
    }

    /// Await the one terminal publication answer. NMP owns fact reduction and
    /// automatically restarts from durable replay if live delivery lags.
    pub async fn result(&self) -> Result<FfiReceiptResult, FfiError> {
        use std::sync::atomic::Ordering;

        if self.reading.swap(true, Ordering::AcqRel) {
            return Err(FfiError::ConcurrentNext);
        }
        let _reading = ReceiptReadingGuard(&self.reading);
        self.restart_replay()?;
        let mut facts = Vec::new();
        loop {
            match self.next_fact().await {
                Ok(Some(fact)) => {
                    let terminal = matches!(fact, nmp::WriteFact::Outcome(_));
                    facts.push(fact);
                    if terminal {
                        let result = nmp::ReceiptResult::from_facts(facts).map_err(|_| {
                            FfiError::ReceiptClosedWithoutOutcome {
                                receipt_id: self.id.0,
                            }
                        })?;
                        return Ok(receipt_result_to_ffi(result));
                    }
                }
                Ok(None) => {
                    return Err(FfiError::ReceiptClosedWithoutOutcome {
                        receipt_id: self.id.0,
                    });
                }
                Err(FfiError::FactStreamLagged { .. }) => {
                    facts.clear();
                    self.restart_replay()?;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Stop delivering live status frames to this stream. The durable receipt
    /// itself is untouched (the write is not cancelled — use
    /// [`NmpEngine::cancel`] for that); a later [`NmpEngine::reattach_receipt`]
    /// traverses the durable history. Safe to call more than once.
    pub fn cancel(&self) {
        let prior = {
            let mut delivery = self.delivery.lock().unwrap();
            match std::mem::replace(&mut *delivery, ReceiptDelivery::Cancelled) {
                ReceiptDelivery::Active { receiver, .. } => Some(receiver),
                ReceiptDelivery::Cancelled => None,
            }
        };
        if let Some(receiver) = prior {
            receiver.close();
        }
    }
}

impl Drop for NmpReceiptStream {
    fn drop(&mut self) {
        self.cancel();
    }
}

struct ReceiptReadingGuard<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for ReceiptReadingGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Release);
    }
}
