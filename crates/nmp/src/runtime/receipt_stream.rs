use std::collections::HashMap;
use std::sync::{mpsc, Arc};

use nmp_grammar::WriteIntent;
use nostr::EventId;

use crate::core::{
    self, Effect, EngineCore, PublishError, ReattachOutcome, ReceiptId, ReceiptReplayCursor,
};
use crate::publish_queue::{ReceiptResult, ReceiptResultError, WriteFact};

use super::{fifo_channel, Cmd, FifoReceiver, FifoRecvError, FifoSender, Handle};

/// Runtime-private identity for one exact live receipt mailbox.
#[derive(Clone)]
pub(super) struct ReceiptDeliveryRegistration(Arc<()>);

impl ReceiptDeliveryRegistration {
    fn new() -> Self {
        Self(Arc::new(()))
    }

    fn is_same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

struct RegisteredReceiptDelivery {
    registration: ReceiptDeliveryRegistration,
    sender: FifoSender<WriteFact>,
    /// Durable replay truth after the last fact this exact FIFO accepted.
    cursor: ReceiptReplayCursor,
}

#[derive(Default)]
pub(super) struct ReceiptDeliveryRegistry {
    by_receipt: HashMap<ReceiptId, Vec<RegisteredReceiptDelivery>>,
}

impl ReceiptDeliveryRegistry {
    pub(super) fn register(
        &mut self,
        id: ReceiptId,
        registration: ReceiptDeliveryRegistration,
        sender: FifoSender<WriteFact>,
        cursor: ReceiptReplayCursor,
    ) {
        self.by_receipt
            .entry(id)
            .or_default()
            .push(RegisteredReceiptDelivery {
                registration,
                sender,
                cursor,
            });
    }

    /// Drop every delivery for a receipt the app has REMOVED (#1039). The
    /// entry is gone, so a stream still pointed at it would wait forever on
    /// facts that can no longer exist.
    pub(super) fn forget(&mut self, id: ReceiptId) {
        self.by_receipt.remove(&id);
    }

    pub(super) fn detach(&mut self, id: ReceiptId, registration: &ReceiptDeliveryRegistration) {
        let Some(deliveries) = self.by_receipt.get_mut(&id) else {
            return;
        };
        deliveries.retain(|delivery| !delivery.registration.is_same(registration));
        if deliveries.is_empty() {
            self.by_receipt.remove(&id);
        }
    }

    pub(super) fn deliver(&mut self, core: &mut EngineCore, id: ReceiptId, status: WriteFact) {
        let Some(deliveries) = self.by_receipt.get_mut(&id) else {
            return;
        };
        deliveries.retain_mut(|delivery| {
            let cursor = core.receipt_cursor_after_status(id, &delivery.cursor, &status);
            if !delivery.sender.send(status.clone()) {
                return false;
            }
            if let Some(cursor) = cursor {
                delivery.cursor = cursor;
            }
            true
        });
        if deliveries.is_empty() {
            self.by_receipt.remove(&id);
        }
    }

    /// Drop terminal producers only after the complete reducer batch has
    /// been delivered, so multiple terminal replay facts from one command
    /// cannot close the FIFO after the first fact.
    pub(super) fn finish_batch(&mut self, core: &EngineCore) {
        self.by_receipt.retain(|id, _| core.receipt_is_live(*id));
    }

    #[cfg(test)]
    pub(super) fn count(&self, id: ReceiptId) -> usize {
        self.by_receipt.get(&id).map_or(0, Vec::len)
    }
}

fn arm_receipt_delivery_close(
    receiver: &FifoReceiver<WriteFact>,
    inbox: mpsc::Sender<Cmd>,
    id: ReceiptId,
    registration: ReceiptDeliveryRegistration,
) {
    receiver.set_close_hook(move || {
        let _ = inbox.send(Cmd::DetachReceiptDelivery { id, registration });
    });
}

pub(super) fn deliver_receipt_replay_page(
    core: &EngineCore,
    deliveries: &mut ReceiptDeliveryRegistry,
    id: ReceiptId,
    sender: FifoSender<WriteFact>,
    registration: ReceiptDeliveryRegistration,
    page: core::ReceiptReplayPage,
) -> (ReattachOutcome, Option<ReceiptReplayCursor>) {
    let outcome = page.outcome;
    let next_cursor = page.next_cursor.clone();
    let accepted_all = page.facts.into_iter().all(|status| sender.send(status));
    if accepted_all
        && outcome == ReattachOutcome::Attached
        && next_cursor.is_none()
        && core.receipt_is_live(id)
    {
        deliveries.register(
            id,
            registration,
            sender,
            page.end_cursor
                .expect("an attached replay page always carries its final cursor"),
        );
    }
    (outcome, next_cursor)
}

/// What acceptance answered: the receipt id the store issued and the event id
/// it froze, or the one typed pre-receipt refusal. Both ids come from the same
/// reducer step, so neither is ever reported without the other.
pub(super) fn publish_result(effects: &[Effect]) -> Result<(ReceiptId, EventId), PublishError> {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::WriteAccepted(id, event_id) => Some(Ok((*id, *event_id))),
            // A correlation-idempotent republish answers with the obligation
            // it resolved to, so its identity is the retained one this page
            // replayed — never the discarded re-composed draft's.
            Effect::ReplayReceipt(id, page) => Some(Ok((
                *id,
                page.frozen_id
                    .expect("an attached replay page always carries its frozen id"),
            ))),
            Effect::PublishFailed(err) => Some(Err(err.clone())),
            _ => None,
        })
        .expect("every publish produces a receipt id or typed allocation failure")
}

pub(super) fn take_publish_replay(
    effects: &mut Vec<Effect>,
) -> Option<(ReceiptId, core::ReceiptReplayPage)> {
    let replay_index = effects
        .iter()
        .position(|effect| matches!(effect, Effect::ReplayReceipt(..)))?;
    let replay = match effects.remove(replay_index) {
        Effect::ReplayReceipt(id, page) => (id, page),
        _ => unreachable!("the located effect is a receipt replay"),
    };
    debug_assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, Effect::ReplayReceipt(..))),
        "one publish can resolve at most one existing receipt"
    );
    Some(replay)
}

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

fn collect_receipt_result(
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

impl Handle {
    /// Enqueue a write. The returned [`ReceiptStream`] carries the stable
    /// store-issued receipt id and the event id acceptance froze, AND streams
    /// every `WriteFact` this intent ever reaches (ledger #9 — enqueue is not
    /// converged; the FIRST value is never a terminal for a durable/
    /// at-most-once intent).
    /// `Ephemeral` also yields receipt facts, but owns no publish queue
    /// obligation or query-visible pending row.
    ///
    /// This synchronous round trip waits only for the local crash-atomic
    /// acceptance door, never for signing, routing, network I/O, or ACKs.
    /// If no pre-acceptance correlation id remains, this returns a typed
    /// error before any stream or identity is fabricated.
    pub fn publish(&self, intent: WriteIntent) -> Result<ReceiptStream, PublishError> {
        let (tx, rx) = fifo_channel();
        let registration = ReceiptDeliveryRegistration::new();
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::PublishTracked {
                intent,
                sender: tx,
                registration: registration.clone(),
                reply: reply_tx,
            })
            .expect("nmp-engine: publish called after shutdown");
        let (id, event_id) = reply_rx
            .recv()
            .expect("nmp-engine: engine dropped publish receipt reply")?;
        arm_receipt_delivery_close(&rx, self.inbox.clone(), id, registration);
        Ok(ReceiptStream {
            id,
            event_id,
            statuses: rx,
            handle: std::panic::AssertUnwindSafe(self.clone()),
        })
    }

    /// Attach to a retained receipt and block until its terminal result.
    /// This is the restart counterpart of [`ReceiptStream::result`].
    pub fn receipt_result(&self, id: ReceiptId) -> Result<ReceiptResult, ReceiptResultError> {
        match self.reattach_receipt(id) {
            ReceiptReattachment::Attached {
                statuses,
                next_cursor,
                ..
            } => collect_receipt_result(self, id, statuses, next_cursor, Vec::new()),
            ReceiptReattachment::NotFound | ReceiptReattachment::RetainedButUnreadable => {
                Err(ReceiptResultError::ReplayUnavailable)
            }
        }
    }

    /// Attach an additional observer to a retained receipt. The returned
    /// channel is primed with durable receipt/attempt facts. Missing and
    /// retained-but-unreadable evidence are distinct outcomes.
    pub fn reattach_receipt(&self, id: ReceiptId) -> ReceiptReattachment {
        self.reattach_receipt_page(id, None)
    }

    /// Continue durable replay from an identity-stable prior-page cursor.
    /// This is delivery mechanism for receipt streams, not a second write
    /// noun.
    pub fn reattach_receipt_from(
        &self,
        id: ReceiptId,
        cursor: ReceiptReplayCursor,
    ) -> ReceiptReattachment {
        self.reattach_receipt_page(id, Some(cursor))
    }

    fn reattach_receipt_page(
        &self,
        id: ReceiptId,
        cursor: Option<ReceiptReplayCursor>,
    ) -> ReceiptReattachment {
        let (tx, rx) = fifo_channel();
        let registration = ReceiptDeliveryRegistration::new();
        arm_receipt_delivery_close(&rx, self.inbox.clone(), id, registration.clone());
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::ReattachReceipt {
                id,
                cursor,
                sender: tx,
                registration,
                reply: reply_tx,
            })
            .expect("nmp-engine: reattach called after shutdown");
        match reply_rx
            .recv()
            .expect("nmp-engine: engine dropped reattach reply")
        {
            (ReattachOutcome::Attached, next_cursor) => ReceiptReattachment::Attached {
                id,
                statuses: rx,
                next_cursor,
            },
            (ReattachOutcome::NotFound, _) => ReceiptReattachment::NotFound,
            (ReattachOutcome::RetainedButUnreadable, _) => {
                ReceiptReattachment::RetainedButUnreadable
            }
        }
    }

    /// #591: recover a receipt after a crash that happened BEFORE the app
    /// could durably record the `ReceiptId` `publish` returned --
    /// looked up by the caller's own correlation token instead. Otherwise
    /// identical to [`Self::reattach_receipt`] (same replay/attach
    /// behavior, same `ReceiptReattachment` outcome vocabulary) -- the
    /// resolved id the caller could not otherwise learn rides along on
    /// `Attached`.
    pub fn reattach_by_correlation(&self, token: String) -> ReceiptReattachment {
        let (tx, rx) = fifo_channel();
        let registration = ReceiptDeliveryRegistration::new();
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::ReattachByCorrelation {
                token,
                sender: tx,
                registration: registration.clone(),
                reply: reply_tx,
            })
            .expect("nmp-engine: reattach called after shutdown");
        match reply_rx
            .recv()
            .expect("nmp-engine: engine dropped reattach reply")
        {
            (ReattachOutcome::Attached, Some(id), next_cursor) => {
                arm_receipt_delivery_close(&rx, self.inbox.clone(), id, registration);
                ReceiptReattachment::Attached {
                    id,
                    statuses: rx,
                    next_cursor,
                }
            }
            (ReattachOutcome::Attached, None, _) => {
                unreachable!(
                    "EngineCore::reattach_by_correlation always resolves an id when Attached"
                )
            }
            (ReattachOutcome::NotFound, _, _) => ReceiptReattachment::NotFound,
            (ReattachOutcome::RetainedButUnreadable, _, _) => {
                ReceiptReattachment::RetainedButUnreadable
            }
        }
    }

    #[cfg(test)]
    pub(super) fn receipt_delivery_count(&self, id: ReceiptId) -> usize {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.inbox
            .send(Cmd::ReceiptDeliveryCount {
                id,
                reply: reply_tx,
            })
            .expect("nmp-engine: receipt delivery census called after shutdown");
        reply_rx
            .recv()
            .expect("nmp-engine: engine dropped receipt delivery census reply")
    }
}

#[cfg(test)]
mod publish_result_tests {
    use super::*;

    #[test]
    fn typed_pre_receipt_failure_is_the_publish_reply() {
        assert_eq!(
            publish_result(&[Effect::PublishFailed(PublishError::NoCurrentAccount)]),
            Err(PublishError::NoCurrentAccount)
        );
        // Custody: `WriteAccepted` is the acceptance answer, and it is not a
        // fact on the stream. `publish()` returning the ids IS the acceptance.
        let frozen = nostr::EventId::from_slice(&[0x5a; 32]).unwrap();
        assert_eq!(
            publish_result(&[Effect::WriteAccepted(ReceiptId(7), frozen)]),
            Ok((ReceiptId(7), frozen))
        );
    }
}

#[cfg(test)]
mod receipt_delivery_lifecycle_tests {
    use super::super::{EngineThread, PoolConfig};
    use super::*;
    use nmp_grammar::{Identity, WriteIntent, WritePayload, WriteRouting};
    use nmp_store::RedbStore;
    use nostr::{Keys, Kind, Timestamp};

    fn parked_write(handle: &Handle, keys: &Keys) -> ReceiptStream {
        handle.set_current_account(Some(keys.public_key()));
        handle
            .publish(WriteIntent {
                payload: WritePayload::Event(nmp_grammar::EventBuilder {
                    kind: Kind::TextNote,
                    tags: (vec![]).into_iter().collect(),
                    content: ("parked receipt delivery lifecycle").into(),
                    created_at: Some(Timestamp::now()),
                }),
                routing: WriteRouting::Auto,
                identity: Identity::Active,
                correlation: None,
            })
            .expect("parked write is accepted")
    }

    /// A receipt without its signer may never emit another live status. Stream
    /// cancel/drop must therefore remove its exact observer immediately,
    /// without relying on a later `notify` call to prune a closed mailbox.
    #[test]
    fn parked_awaiting_capability_reattach_cancel_does_not_retain_deliveries() {
        let (thread, handle) = EngineThread::spawn(
            RedbStore::temporary().expect("temporary Redb store"),
            10,
            PoolConfig::default(),
        )
        .expect("test engine thread construction");
        let tracked = parked_write(&handle, &Keys::generate());
        let id = tracked.id;
        assert_eq!(handle.receipt_delivery_count(id), 1);

        tracked.statuses.close();
        assert_eq!(
            handle.receipt_delivery_count(id),
            0,
            "closing the original publish stream withdraws its observer"
        );

        for iteration in 0..128 {
            let statuses = match handle.reattach_receipt(id) {
                ReceiptReattachment::Attached {
                    statuses,
                    next_cursor: None,
                    ..
                } => statuses,
                ReceiptReattachment::Attached { .. } => {
                    panic!("two retained facts fit in one replay page")
                }
                ReceiptReattachment::NotFound | ReceiptReattachment::RetainedButUnreadable => {
                    panic!("parked retained receipt remains readable")
                }
            };
            assert_eq!(
                handle.receipt_delivery_count(id),
                1,
                "each fresh reattachment owns exactly one live observer"
            );
            if iteration % 2 == 0 {
                statuses.close();
            } else {
                drop(statuses);
            }
            assert_eq!(
                handle.receipt_delivery_count(id),
                0,
                "cancel/drop detaches before the next engine command"
            );
        }

        handle.shutdown();
        thread.join();
    }
}
