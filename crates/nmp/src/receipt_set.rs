//! Bounded, fair observation of a caller-retained receipt set (#961).
//!
//! This is composition over the existing per-receipt replay/live handles.
//! It deliberately owns no reducer callback, receipt store, retry policy, or
//! second live-sink registry. One pull future polls every finite receiver in
//! round-robin order; cancellation closes them all.

use std::collections::HashSet;
use std::future::poll_fn;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::task::{Context, Poll};

use crate::{
    engine::ReceiptObservationAccess, Engine, FifoNextError, FifoReceiver, ReceiptId,
    ReceiptReattachment, ReceiptReplayCursor, WriteStatus,
};

/// Maximum identities accepted by one receipt-set observation.
pub const RECEIPT_SET_CAPACITY: usize = 32;

/// One caller-retained receipt identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ReceiptIdentity {
    Id(ReceiptId),
    Correlation(String),
}

/// Typed admission refusal before any receipt observer is attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiptSetError {
    CapacityExceeded { capacity: usize, requested: usize },
    DuplicateIdentity { identity: ReceiptIdentity },
    EngineClosed,
}

impl std::fmt::Display for ReceiptSetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CapacityExceeded {
                capacity,
                requested,
            } => write!(
                f,
                "receipt set capacity {capacity} exceeded by {requested} requested identities"
            ),
            Self::DuplicateIdentity { identity } => {
                write!(f, "duplicate receipt identity {identity:?}")
            }
            Self::EngineClosed => write!(f, "engine already shut down"),
        }
    }
}

impl std::error::Error for ReceiptSetError {}

/// One tagged outcome from a receipt-set observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiptSetEvent {
    Fact {
        identity: ReceiptIdentity,
        receipt_id: ReceiptId,
        status: WriteStatus,
    },
    NotFound {
        identity: ReceiptIdentity,
    },
    RetainedButUnreadable {
        identity: ReceiptIdentity,
        receipt_id: Option<ReceiptId>,
    },
    /// The finite live FIFO lagged. NMP has already started a fresh durable
    /// replay for this identity; following `Fact`s are that replay.
    ReplayAfterLag {
        identity: ReceiptIdentity,
        receipt_id: ReceiptId,
    },
    /// A previously attached receipt could no longer be reconstructed.
    ReplayUnavailable {
        identity: ReceiptIdentity,
        receipt_id: ReceiptId,
    },
    /// This receipt's complete replay/live stream has closed. No earlier
    /// queued fact for this identity remains hidden behind this marker.
    Closed {
        identity: ReceiptIdentity,
        receipt_id: ReceiptId,
    },
}

/// Misuse of the single-consumer pull handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptSetNextError {
    ConcurrentNext,
}

enum EntryDelivery {
    Active {
        receiver: FifoReceiver<WriteStatus>,
        next_cursor: Option<ReceiptReplayCursor>,
    },
    Pending(ReceiptSetEvent),
    Finished,
}

struct Entry {
    identity: ReceiptIdentity,
    receipt_id: Option<ReceiptId>,
    delivery: EntryDelivery,
}

struct State {
    entries: Vec<Entry>,
    next_index: usize,
    cancelled: bool,
}

/// One pull-based observation over a finite retained receipt set.
pub struct ReceiptSetSubscription {
    engine: ReceiptObservationAccess,
    state: Mutex<State>,
    reading: AtomicBool,
}

impl Engine {
    /// Observe a bounded caller-supplied receipt set through one fair pull
    /// sequence. Exact capacity succeeds; capacity + 1 refuses before any
    /// per-receipt attachment is created.
    pub fn observe_receipts(
        &self,
        identities: Vec<ReceiptIdentity>,
    ) -> Result<ReceiptSetSubscription, ReceiptSetError> {
        if identities.len() > RECEIPT_SET_CAPACITY {
            return Err(ReceiptSetError::CapacityExceeded {
                capacity: RECEIPT_SET_CAPACITY,
                requested: identities.len(),
            });
        }
        let mut unique = HashSet::with_capacity(identities.len());
        for identity in &identities {
            if !unique.insert(identity.clone()) {
                return Err(ReceiptSetError::DuplicateIdentity {
                    identity: identity.clone(),
                });
            }
        }

        let access = self.receipt_observation_access();
        let mut entries = Vec::with_capacity(identities.len());
        for identity in identities {
            entries.push(Entry::attach(&access, identity)?);
        }
        Ok(ReceiptSetSubscription {
            engine: access,
            state: Mutex::new(State {
                entries,
                next_index: 0,
                cancelled: false,
            }),
            reading: AtomicBool::new(false),
        })
    }
}

impl Entry {
    fn attach(
        engine: &ReceiptObservationAccess,
        identity: ReceiptIdentity,
    ) -> Result<Self, ReceiptSetError> {
        let result = match &identity {
            ReceiptIdentity::Id(id) => engine.reattach(*id),
            ReceiptIdentity::Correlation(token) => engine.reattach_by_correlation(token.clone()),
        }
        .map_err(|_| ReceiptSetError::EngineClosed)?;
        Ok(Self::from_reattachment(identity, result))
    }

    fn from_reattachment(identity: ReceiptIdentity, result: ReceiptReattachment) -> Self {
        match result {
            ReceiptReattachment::Attached {
                id,
                statuses,
                next_cursor,
            } => Self {
                identity,
                receipt_id: Some(id),
                delivery: EntryDelivery::Active {
                    receiver: statuses,
                    next_cursor,
                },
            },
            ReceiptReattachment::NotFound => Self {
                identity: identity.clone(),
                receipt_id: None,
                delivery: EntryDelivery::Pending(ReceiptSetEvent::NotFound { identity }),
            },
            ReceiptReattachment::RetainedButUnreadable => {
                let receipt_id = match identity {
                    ReceiptIdentity::Id(id) => Some(id),
                    ReceiptIdentity::Correlation(_) => None,
                };
                Self {
                    identity: identity.clone(),
                    receipt_id,
                    delivery: EntryDelivery::Pending(ReceiptSetEvent::RetainedButUnreadable {
                        identity,
                        receipt_id,
                    }),
                }
            }
        }
    }

    fn continue_page(&mut self, engine: &ReceiptObservationAccess, cursor: ReceiptReplayCursor) {
        let Some(id) = self.receipt_id else {
            self.delivery = EntryDelivery::Finished;
            return;
        };
        self.delivery = match engine.reattach_from(id, cursor) {
            Ok(ReceiptReattachment::Attached {
                id: attached_id,
                statuses,
                next_cursor,
            }) if attached_id == id => EntryDelivery::Active {
                receiver: statuses,
                next_cursor,
            },
            Ok(ReceiptReattachment::Attached { .. })
            | Ok(ReceiptReattachment::NotFound)
            | Ok(ReceiptReattachment::RetainedButUnreadable)
            | Err(_) => EntryDelivery::Pending(ReceiptSetEvent::ReplayUnavailable {
                identity: self.identity.clone(),
                receipt_id: id,
            }),
        };
    }

    fn recover_after_lag(&mut self, engine: &ReceiptObservationAccess) -> ReceiptSetEvent {
        let id = self
            .receipt_id
            .expect("only an attached receipt receiver can report lag");
        let event = ReceiptSetEvent::ReplayAfterLag {
            identity: self.identity.clone(),
            receipt_id: id,
        };
        let identity = self.identity.clone();
        *self = match Self::attach(engine, identity.clone()) {
            Ok(entry) => entry,
            Err(_) => Self {
                identity: identity.clone(),
                receipt_id: Some(id),
                delivery: EntryDelivery::Pending(ReceiptSetEvent::ReplayUnavailable {
                    identity,
                    receipt_id: id,
                }),
            },
        };
        event
    }
}

impl ReceiptSetSubscription {
    /// Pull the next fair tagged outcome, or `None` after cancellation or
    /// after every requested identity is terminal/absent/unreadable.
    pub async fn next(&self) -> Result<Option<ReceiptSetEvent>, ReceiptSetNextError> {
        if self.reading.swap(true, Ordering::AcqRel) {
            return Err(ReceiptSetNextError::ConcurrentNext);
        }
        let _reading = ReadingGuard(&self.reading);
        Ok(poll_fn(|cx| self.poll_next(cx)).await)
    }

    fn poll_next(&self, cx: &mut Context<'_>) -> Poll<Option<ReceiptSetEvent>> {
        let mut state = self.state.lock().unwrap();
        if state.cancelled || state.entries.is_empty() {
            return Poll::Ready(None);
        }
        let count = state.entries.len();
        for offset in 0..count {
            let index = (state.next_index + offset) % count;
            let entry = &mut state.entries[index];
            let outcome = match &mut entry.delivery {
                EntryDelivery::Pending(_) => {
                    let EntryDelivery::Pending(event) =
                        std::mem::replace(&mut entry.delivery, EntryDelivery::Finished)
                    else {
                        unreachable!()
                    };
                    Some(event)
                }
                EntryDelivery::Finished => None,
                EntryDelivery::Active {
                    receiver,
                    next_cursor,
                } => match receiver.poll_recv(cx) {
                    Poll::Ready(Ok(Some(status))) => Some(ReceiptSetEvent::Fact {
                        identity: entry.identity.clone(),
                        receipt_id: entry.receipt_id.expect("attached receipt has an id"),
                        status,
                    }),
                    Poll::Ready(Err(FifoNextError::Lagged)) => {
                        Some(entry.recover_after_lag(&self.engine))
                    }
                    Poll::Ready(Err(FifoNextError::ConcurrentNext)) => {
                        unreachable!("receipt-set owns each private receiver")
                    }
                    Poll::Ready(Ok(None)) => {
                        if let Some(cursor) = next_cursor.take() {
                            entry.continue_page(&self.engine, cursor);
                            // The finite next page was filled synchronously
                            // before its receiver was installed here, so no
                            // sender observed this future's waker.
                            cx.waker().wake_by_ref();
                            None
                        } else {
                            entry.delivery = EntryDelivery::Finished;
                            Some(ReceiptSetEvent::Closed {
                                identity: entry.identity.clone(),
                                receipt_id: entry.receipt_id.expect("attached receipt has an id"),
                            })
                        }
                    }
                    Poll::Pending => None,
                },
            };
            if let Some(event) = outcome {
                state.next_index = (index + 1) % count;
                return Poll::Ready(Some(event));
            }
        }
        if state
            .entries
            .iter()
            .all(|entry| matches!(entry.delivery, EntryDelivery::Finished))
        {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }

    /// Withdraw every per-receipt live attachment. Durable obligations remain.
    pub fn cancel(&self) {
        let mut state = self.state.lock().unwrap();
        if state.cancelled {
            return;
        }
        state.cancelled = true;
        for entry in &mut state.entries {
            if let EntryDelivery::Active { receiver, .. } = &entry.delivery {
                receiver.close();
            }
            entry.delivery = EntryDelivery::Finished;
        }
    }
}

impl Drop for ReceiptSetSubscription {
    fn drop(&mut self) {
        self.cancel();
    }
}

struct ReadingGuard<'a>(&'a AtomicBool);

impl Drop for ReadingGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Durability, EngineConfig, WriteIntent, WritePayload, WriteRouting};
    use nostr::{Keys, Kind, Timestamp, UnsignedEvent};
    use std::sync::Arc;

    fn parked(engine: &Arc<Engine>, seed: u64, correlation: Option<&str>) -> ReceiptId {
        let keys = Keys::generate();
        engine
            .set_active_account(Some(keys.public_key()))
            .expect("engine open");
        let receipt = engine
            .publish_tracked(WriteIntent {
                payload: WritePayload::Unsigned(UnsignedEvent::new(
                    keys.public_key(),
                    Timestamp::from(seed),
                    Kind::TextNote,
                    Vec::new(),
                    format!("receipt set {seed}"),
                )),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
                identity_override: None,
                correlation: correlation.map(|token| token.try_into().expect("valid token")),
            })
            .expect("accepted");
        let id = receipt.id;
        drop(receipt);
        id
    }

    #[tokio::test]
    async fn four_parked_receipts_do_not_starve_a_fifth_closed_receipt() {
        let engine = Arc::new(Engine::new(EngineConfig::default()).expect("engine"));
        let mut ids = (0..5)
            .map(|index| parked(&engine, index + 1, None))
            .collect::<Vec<_>>();
        let terminal = ids.pop().unwrap();
        engine.cancel(terminal).expect("cancel terminal receipt");
        let set = engine
            .observe_receipts(
                ids.iter()
                    .copied()
                    .chain(std::iter::once(terminal))
                    .map(ReceiptIdentity::Id)
                    .collect(),
            )
            .expect("set opens");

        let mut saw_terminal_fact = false;
        let mut saw_terminal_close = false;
        for _ in 0..16 {
            let event = tokio::time::timeout(std::time::Duration::from_secs(1), set.next())
                .await
                .expect("fair pull must not park behind A-D")
                .expect("single reader")
                .expect("set remains live");
            match event {
                ReceiptSetEvent::Fact {
                    receipt_id,
                    status: WriteStatus::Cancelled,
                    ..
                } if receipt_id == terminal => saw_terminal_fact = true,
                ReceiptSetEvent::Closed { receipt_id, .. } if receipt_id == terminal => {
                    assert!(
                        saw_terminal_fact,
                        "Closed must follow the complete terminal replay"
                    );
                    saw_terminal_close = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_terminal_close, "parked heads must not starve receipt E");
        set.cancel();
        engine.shutdown();
    }

    #[test]
    fn exact_capacity_succeeds_and_plus_one_refuses_before_attachment() {
        let engine = Arc::new(Engine::new(EngineConfig::default()).expect("engine"));
        let exact = (1..=RECEIPT_SET_CAPACITY as u64)
            .map(|id| ReceiptIdentity::Id(ReceiptId(id)))
            .collect();
        assert!(engine.observe_receipts(exact).is_ok());
        let too_many = (1..=RECEIPT_SET_CAPACITY as u64 + 1)
            .map(|id| ReceiptIdentity::Id(ReceiptId(id)))
            .collect();
        assert!(matches!(
            engine.observe_receipts(too_many),
            Err(ReceiptSetError::CapacityExceeded {
                capacity: RECEIPT_SET_CAPACITY,
                requested
            }) if requested == RECEIPT_SET_CAPACITY + 1
        ));
        engine.shutdown();
    }

    #[tokio::test]
    async fn correlation_identity_reports_the_resolved_receipt_id() {
        let fixture = tempfile::tempdir().expect("temporary directory");
        let path = fixture.path().join("receipt-set-restart.redb");
        let engine = Arc::new(
            Engine::new(EngineConfig {
                store_path: Some(path.to_string_lossy().into_owned()),
                ..EngineConfig::default()
            })
            .expect("engine"),
        );
        let expected = parked(&engine, 99, Some("receipt-set-correlation"));
        engine.shutdown();
        drop(engine);
        let engine = Arc::new(
            Engine::new(EngineConfig {
                store_path: Some(path.to_string_lossy().into_owned()),
                ..EngineConfig::default()
            })
            .expect("restart engine"),
        );
        let set = engine
            .observe_receipts(vec![ReceiptIdentity::Correlation(
                "receipt-set-correlation".to_string(),
            )])
            .expect("set opens");
        let event = set.next().await.expect("single reader").expect("fact");
        assert!(matches!(
            event,
            ReceiptSetEvent::Fact { receipt_id, .. } if receipt_id == expected
        ));
        set.cancel();
        engine.shutdown();
    }

    #[test]
    fn one_cancel_surface_detaches_every_composed_live_sink() {
        let engine = Arc::new(Engine::new(EngineConfig::default()).expect("engine"));
        let ids = (0..4)
            .map(|index| parked(&engine, index + 200, None))
            .collect::<Vec<_>>();
        let set = engine
            .observe_receipts(ids.iter().copied().map(ReceiptIdentity::Id).collect())
            .expect("set opens");
        for id in &ids {
            assert_eq!(engine.receipt_sink_count(*id).unwrap(), 1);
        }
        set.cancel();
        for id in ids {
            assert_eq!(engine.receipt_sink_count(id).unwrap(), 0);
        }
        engine.shutdown();
    }
}
