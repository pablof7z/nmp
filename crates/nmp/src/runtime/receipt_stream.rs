use nostr::EventId;

use crate::core::{ReceiptId, ReceiptReplayCursor};
use crate::publish_queue::{ReceiptResult, ReceiptResultError, WriteFact};

use super::{FifoReceiver, FifoRecvError, Handle};

/// A newly accepted write's stable store-issued identity plus its live
/// observer. Keeping the id separate from the channel lets a later process
/// call [`Handle::reattach_receipt`] without replaying acceptance.
pub struct ReceiptStream {
    pub id: ReceiptId,
    /// The event id acceptance FROZE — this write's identity from acceptance
    /// onward, unchanged by signing (a NIP-01 id never depends on `sig`) and
    /// already re-derived against the row a replaceable edit's acceptance
    /// CAS-ed, so it is the post-restamp value in every case.
    ///
    /// It is here because the same transaction that issued `id` decided it.
    /// Before #903, the alternative was reading it back out of the old
    /// zero-argument queue enumeration, which materialized the whole retained
    /// receipt set. Queue inspection is now bounded, and an event-id lookup
    /// is direct, but acceptance still must return the value it just decided.
    pub event_id: EventId,
    pub statuses: FifoReceiver<WriteFact>,
    pub(super) handle: std::panic::AssertUnwindSafe<Handle>,
}

impl ReceiptStream {
    /// Block until this accepted write reaches its one terminal result.
    ///
    /// NMP performs fact reduction and durable replay itself. If the finite
    /// live FIFO lags, collection restarts from the retained receipt rather
    /// than asking the app to understand replay cursors.
    pub fn result(self) -> Result<ReceiptResult, ReceiptResultError> {
        self.handle.0.receipt_result(self.id)
    }
}

pub(super) fn collect_receipt_result(
    handle: &Handle,
    id: ReceiptId,
    mut statuses: FifoReceiver<WriteFact>,
    mut next_cursor: Option<ReceiptReplayCursor>,
    mut facts: Vec<WriteFact>,
) -> Result<ReceiptResult, ReceiptResultError> {
    loop {
        match statuses.recv() {
            Ok(fact) => {
                let terminal = matches!(fact, WriteFact::Outcome(_));
                facts.push(fact);
                if terminal {
                    return ReceiptResult::from_facts(facts);
                }
            }
            Err(FifoRecvError::Closed) => {
                let Some(cursor) = next_cursor.take() else {
                    return Err(ReceiptResultError::ClosedWithoutOutcome);
                };
                match handle.reattach_receipt_from(id, cursor) {
                    ReceiptReattachment::Attached {
                        statuses: page,
                        next_cursor: cursor,
                        ..
                    } => {
                        statuses = page;
                        next_cursor = cursor;
                    }
                    ReceiptReattachment::NotFound | ReceiptReattachment::RetainedButUnreadable => {
                        return Err(ReceiptResultError::ReplayUnavailable);
                    }
                }
            }
            Err(FifoRecvError::Lagged) => {
                facts.clear();
                match handle.reattach_receipt(id) {
                    ReceiptReattachment::Attached {
                        statuses: page,
                        next_cursor: cursor,
                        ..
                    } => {
                        statuses = page;
                        next_cursor = cursor;
                    }
                    ReceiptReattachment::NotFound | ReceiptReattachment::RetainedButUnreadable => {
                        return Err(ReceiptResultError::ReplayUnavailable);
                    }
                }
            }
        }
    }
}

/// Result of looking up retained receipt facts by stable id (or, #591, by a
/// caller correlation token translated to one).
pub enum ReceiptReattachment {
    /// The observer is attached and this channel is already primed with all
    /// readable retained facts. Carries the resolved [`ReceiptId`] -- for
    /// [`Handle::reattach_receipt`] this is simply the caller's own input
    /// echoed back; for [`Handle::reattach_by_correlation`] (#591) it is the
    /// id the token resolved to, which the caller could not otherwise learn.
    Attached {
        id: ReceiptId,
        statuses: FifoReceiver<WriteFact>,
        /// Identity-stable durable-replay continuation for the next finite
        /// page. `None` means this receiver is caught up and attached to
        /// live work.
        next_cursor: Option<ReceiptReplayCursor>,
    },
    /// No retained receipt with this id (or token) exists.
    NotFound,
    /// The id is retained, but durable receipt or attempt evidence is corrupt
    /// or unreadable. The obligation remains untouched and nothing publishes.
    RetainedButUnreadable,
}
