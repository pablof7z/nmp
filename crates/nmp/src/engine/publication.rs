use nmp_grammar::WriteIntent;
use nostr::EventId;

use super::Engine;
use crate::error::EngineError;
use nmp_engine::core::ReceiptId;
use nmp_engine::publish_queue::{
    PublishQueueEntry, PublishQueueReadError, ReceiptResult, ReceiptResultError,
    RemoveQueueEntryError,
};
use nmp_runtime::{ReceiptReattachment, ReceiptReplayCursor, ReceiptStream};

/// The only successful result from explicit pre-signature cancellation.
/// The closed success type cannot carry a status that cancellation did not
/// commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelWriteOutcome {
    Cancelled,
}

/// Typed refusal from explicit pre-signature write cancellation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelWriteError {
    UnknownReceipt {
        receipt_id: ReceiptId,
    },
    AlreadySigned {
        receipt_id: ReceiptId,
        event_id: EventId,
    },
    AlreadyCompensated {
        receipt_id: ReceiptId,
    },
    AlreadySuperseded {
        receipt_id: ReceiptId,
    },
    /// The write was refused at acceptance and is already a permanently
    /// failed queue entry. There is nothing to cancel; remove it instead.
    AlreadyRefused {
        receipt_id: ReceiptId,
    },
    PersistenceFailed {
        receipt_id: ReceiptId,
        reason: String,
    },
    EngineClosed,
}

fn cancel_write_outcome_from_engine(
    outcome: nmp_engine::publish_queue::CancelWriteOutcome,
) -> CancelWriteOutcome {
    match outcome {
        nmp_engine::publish_queue::CancelWriteOutcome::Cancelled => CancelWriteOutcome::Cancelled,
    }
}

fn cancel_write_error_from_engine(
    error: nmp_engine::publish_queue::CancelWriteError,
) -> CancelWriteError {
    match error {
        nmp_engine::publish_queue::CancelWriteError::UnknownReceipt { receipt_id } => {
            CancelWriteError::UnknownReceipt { receipt_id }
        }
        nmp_engine::publish_queue::CancelWriteError::AlreadySigned {
            receipt_id,
            event_id,
        } => CancelWriteError::AlreadySigned {
            receipt_id,
            event_id,
        },
        nmp_engine::publish_queue::CancelWriteError::AlreadyCompensated { receipt_id } => {
            CancelWriteError::AlreadyCompensated { receipt_id }
        }
        nmp_engine::publish_queue::CancelWriteError::AlreadySuperseded { receipt_id } => {
            CancelWriteError::AlreadySuperseded { receipt_id }
        }
        nmp_engine::publish_queue::CancelWriteError::AlreadyRefused { receipt_id } => {
            CancelWriteError::AlreadyRefused { receipt_id }
        }
        nmp_engine::publish_queue::CancelWriteError::PersistenceFailed { receipt_id, reason } => {
            CancelWriteError::PersistenceFailed { receipt_id, reason }
        }
        nmp_engine::publish_queue::CancelWriteError::EngineClosed => CancelWriteError::EngineClosed,
    }
}

impl std::fmt::Display for CancelWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownReceipt { receipt_id } => write!(f, "unknown receipt {}", receipt_id.0),
            Self::AlreadySigned {
                receipt_id,
                event_id,
            } => write!(
                f,
                "receipt {} is already signed as {event_id}",
                receipt_id.0
            ),
            Self::AlreadyCompensated { receipt_id } => {
                write!(f, "receipt {} is already compensated", receipt_id.0)
            }
            Self::AlreadySuperseded { receipt_id } => {
                write!(
                    f,
                    "receipt {} was superseded by a newer write",
                    receipt_id.0
                )
            }
            Self::AlreadyRefused { receipt_id } => {
                write!(f, "receipt {} was refused at acceptance", receipt_id.0)
            }
            Self::PersistenceFailed { receipt_id, reason } => write!(
                f,
                "could not persist cancellation for receipt {}: {reason}",
                receipt_id.0
            ),
            Self::EngineClosed => f.write_str("engine already shut down"),
        }
    }
}

impl std::error::Error for CancelWriteError {}

impl Engine {
    /// Noun 2: enqueue a write -- the call itself never blocks on routing/
    /// wire/ack, but its return value is not fire-and-forget: the returned
    /// [`ReceiptStream`] is the caller's one way to observe how the intent
    /// resolved, and every `WriteFact` it ever reaches streams through it
    /// (ledger #9 -- enqueue is not converged). Returning `Ok` IS
    /// acceptance, so there is no acceptance fact on the stream. A tampered
    /// `WritePayload::Signed` cannot resolve, so it is refused by this call
    /// itself and nothing is taken into custody -- see the parent facade
    /// module's doc.
    ///
    /// The receipt carries the stable store-issued
    /// [`ReceiptId`](crate::ReceiptId) that process-later reattachment
    /// needs, AND the event id acceptance froze
    /// ([`ReceiptStream::event_id`]) — the write's identity from acceptance
    /// onward, post-restamp in every case, and the same value
    /// [`Self::publish_queue`] later reports for that receipt. One
    /// transaction decided both, so acceptance never hands back less than the
    /// whole receipt (#1314). Pre-acceptance correlation-id exhaustion
    /// returns a typed error without creating a receipt at all.
    ///
    /// Identity (#47): with [`crate::Identity::Active`] — the default — a builder
    /// payload signs as the current account, and fails closed pre-acceptance
    /// when there is no current account (nothing is pinned, so nothing may
    /// park). [`crate::Identity::Explicit`] is explicit per-write consent to
    /// publish as that key — whether or not it is current — without
    /// touching the current account: it works even while logged out, and
    /// acceptance pins the key so later [`Self::make_current_account`] calls
    /// cannot retarget the write. A named key with no available signing
    /// provider parks durably as
    /// [`SigningState::AwaitingSigner`](crate::SigningState) until that
    /// exact key's configured provider becomes available. On a `Signed` payload the author is
    /// already frozen in the bytes, so an explicit identity may only
    /// RESTATE it: naming anybody else cannot resolve, so this call refuses
    /// it and takes nothing into custody.
    pub fn publish(&self, intent: WriteIntent) -> Result<ReceiptStream, EngineError> {
        let handle = {
            let guard = self
                .inner
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            match &*guard {
                Some(inner) => inner.handle.clone(),
                None => return Err(EngineError::EngineClosed),
            }
        };
        handle
            .publish(intent)
            .map_err(EngineError::from_publish_error)
    }

    /// Reattach to durable receipt facts after a restart. Missing ids and
    /// retained obligations with unreadable evidence are distinct outcomes.
    pub fn reattach_receipt(&self, id: ReceiptId) -> Result<ReceiptReattachment, EngineError> {
        self.with_handle(|handle| handle.reattach_receipt(id))
    }

    /// Reattach after a restart and wait for NMP's terminal publication
    /// result without exposing replay pages or fact reduction to the app.
    pub fn receipt_result(&self, id: ReceiptId) -> Result<ReceiptResult, ReceiptResultError> {
        match self.with_handle(|handle| handle.receipt_result(id)) {
            Ok(result) => result,
            Err(_) => Err(ReceiptResultError::ReplayUnavailable),
        }
    }

    #[doc(hidden)]
    pub fn reattach_receipt_from(
        &self,
        id: ReceiptId,
        cursor: ReceiptReplayCursor,
    ) -> Result<ReceiptReattachment, EngineError> {
        self.with_handle(|handle| handle.reattach_receipt_from(id, cursor))
    }

    /// #591: recover a receipt after a crash that happened BEFORE the app
    /// could durably persist the `ReceiptId` `publish` returned --
    /// looked up by the caller's own crash-safe correlation token instead.
    /// Otherwise identical to [`Self::reattach_receipt`].
    pub fn reattach_by_correlation(
        &self,
        token: String,
    ) -> Result<ReceiptReattachment, EngineError> {
        self.with_handle(|handle| handle.reattach_by_correlation(token))
    }

    /// Read one bounded page of the app's own publish queue (#903/#1039).
    ///
    /// Every write NMP still holds a receipt for, with what it knows about
    /// each one right now: signing state, the intended destination set and
    /// whether it is closed, per-relay state, the whole-write outcome if it
    /// has one, and any latched persistence fault.
    ///
    /// INSPECTION, never waiting. Nothing here blocks on settlement, and a
    /// locally accepted write is already visible through the app's own live
    /// query long before it appears here as settled.
    ///
    /// `after` is an exclusive stable receipt-id cursor. `limit` is a `u8`
    /// so one request can never materialize more than 255 complete entries.
    pub fn publish_queue(
        &self,
        after: Option<ReceiptId>,
        limit: u8,
    ) -> Result<Vec<PublishQueueEntry>, PublishQueueReadError> {
        self.with_handle(|handle| handle.publish_queue_entries(after, limit))
            .map_err(|_| PublishQueueReadError::EngineClosed)?
    }

    /// Reach the currently open write obligations for one event id (#903).
    ///
    /// A LiveQuery row already carries this id. The result contains no event
    /// content and no terminal receipt history: it is the exact join from
    /// that row to each active `ReceiptId`, whose retained-plus-live facts the
    /// app can observe with [`Self::reattach_receipt`]. More than one receipt
    /// can own identical event bytes, so the result is bounded and paged
    /// rather than choosing one and hiding the rest.
    pub fn publish_queue_for_event(
        &self,
        event_id: EventId,
        after: Option<ReceiptId>,
        limit: u8,
    ) -> Result<Vec<PublishQueueEntry>, PublishQueueReadError> {
        self.with_handle(|handle| handle.publish_queue_entries_for_event(event_id, after, limit))
            .map_err(|_| PublishQueueReadError::EngineClosed)?
    }

    /// Forget one queue entry (#1039).
    ///
    /// A real TERMINATION path: a write parked forever on a signer that
    /// never attached, and a permanently-failed refused entry, end no other
    /// way. An entry whose obligation is still open is refused — [`Self::cancel`]
    /// it first, then remove the terminal receipt cancellation leaves behind.
    /// That pair is the whole termination path for a signer-parked write:
    /// cancelling ends the obligation and compensates the optimistic row the
    /// write promised, and removal forgets the receipt.
    ///
    /// This does NOT close #46. Retained receipts and correlation tokens
    /// still regrow without bound; enumerating them is what makes the growth
    /// visible.
    pub fn remove_publish_queue_entry(&self, id: ReceiptId) -> Result<(), RemoveQueueEntryError> {
        self.with_handle(|handle| handle.remove_publish_queue_entry(id))
            .map_err(|_| RemoveQueueEntryError::EngineClosed)?
    }

    /// Explicitly cancel one accepted unsigned write by its stable receipt
    /// id. [`CancelWriteOutcome::Cancelled`] means the durable
    /// not-sent fact committed; signed or otherwise terminal receipts return
    /// a precise typed refusal.
    pub fn cancel(&self, id: ReceiptId) -> Result<CancelWriteOutcome, CancelWriteError> {
        self.with_handle(|handle| handle.cancel_write(id))
            .map_err(|_| CancelWriteError::EngineClosed)?
            .map(cancel_write_outcome_from_engine)
            .map_err(cancel_write_error_from_engine)
    }
}
