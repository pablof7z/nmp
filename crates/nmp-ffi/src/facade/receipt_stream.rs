use std::sync::{Arc, Mutex};

#[cfg(doc)]
use super::NmpEngine;
use crate::convert::{receipt_result_to_ffi, write_status_to_ffi, FfiError, WriteStatusRef};
use crate::types::{FfiReceiptResult, FfiWriteFact};
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
