//! Native projection of NMP's optional NIP-02 following resource/action.
//! This module only mirrors Rust-owned state and drains Rust-owned streams;
//! no contact-list parsing, replacement composition, readiness policy, or
//! optimistic following boolean lives at the FFI boundary.

use std::sync::Arc;

use crate::convert::{write_status_to_ffi, FfiError, WriteStatusRef};
use crate::types::FfiWriteFact;
use nmp_nip02::{
    AsyncFollowObservation, FollowActionFailure, FollowAvailability, FollowRelationship,
    FollowSnapshot,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiFollowRelationship {
    Unknown,
    NotFollowing,
    Following,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiFollowAvailability {
    SignedOut,
    Acquiring,
    Ready,
    NoContactList,
    CachedOnly,
    SourceUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FfiFollowSnapshot {
    pub current_pubkey: Option<String>,
    pub target: String,
    pub relationship: FfiFollowRelationship,
    pub availability: FfiFollowAvailability,
    pub base_event_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiFollowActionFailure {
    InvalidTarget { got: String },
    SignedOut,
    EngineClosed,
    ReceiptUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiFollowActionStatus {
    Receipt {
        receipt_id: u64,
        status: FfiWriteFact,
    },
    Failed {
        failure: FfiFollowActionFailure,
    },
}

/// Pull-based following-relationship observation handle (#680). Each `next()`
/// awaits the engine's waker-driven async row mailbox and folds a complete
/// self-contained relationship snapshot inline — no NMP-owned OS thread per
/// observation. `None` is the terminal signal (the demand was withdrawn or the
/// engine shut down). `Drop`/`cancel` withdraw the observation.
#[derive(uniffi::Object)]
pub struct NmpFollowStream {
    inner: AsyncFollowObservation,
}

impl NmpFollowStream {
    pub(crate) fn new(inner: AsyncFollowObservation) -> Arc<Self> {
        Arc::new(Self { inner })
    }
}

#[uniffi::export]
impl NmpFollowStream {
    /// Await the next relationship snapshot, or `None` once the observation is
    /// withdrawn. A second concurrent `next()` is [`FfiError::ConcurrentNext`].
    pub async fn next(&self) -> Result<Option<FfiFollowSnapshot>, FfiError> {
        match self.inner.next().await {
            Ok(Some(snapshot)) => Ok(Some(snapshot_to_ffi(snapshot))),
            Ok(None) => Ok(None),
            Err(_) => Err(FfiError::ConcurrentNext),
        }
    }

    pub fn cancel(&self) {
        self.inner.cancel();
    }
}

impl Drop for NmpFollowStream {
    fn drop(&mut self) {
        self.inner.cancel();
    }
}

/// Pull-based follow/unfollow receipt projection. It owns no action worker,
/// retry policy, cancellation state, or durable lifecycle: successful actions
/// read the ordinary receipt FIFO directly; an immediate typed refusal is one
/// preloaded terminal fact. `None` is the terminal signal.
#[derive(uniffi::Object)]
pub struct NmpFollowActionStream {
    delivery: FollowActionDelivery,
}

enum FollowActionDelivery {
    Receipt {
        receipt_id: u64,
        statuses: nmp::AsyncFifoReceiver<nmp::WriteFact>,
    },
    Immediate {
        statuses: nmp::AsyncFifoReceiver<FfiFollowActionStatus>,
    },
}

impl NmpFollowActionStream {
    pub(crate) fn from_receipt(receipt: nmp::ReceiptStream) -> Arc<Self> {
        Arc::new(Self {
            delivery: FollowActionDelivery::Receipt {
                receipt_id: receipt.id.0,
                statuses: receipt.statuses.into_async(),
            },
        })
    }

    pub(crate) fn one_shot_failure(failure: FfiFollowActionFailure) -> Arc<Self> {
        let (sender, statuses) = nmp::fifo_channel();
        sender.send(FfiFollowActionStatus::Failed { failure });
        Arc::new(Self {
            delivery: FollowActionDelivery::Immediate {
                statuses: statuses.into_async(),
            },
        })
    }
}

#[uniffi::export]
impl NmpFollowActionStream {
    /// Await the next follow-action status in order, or `None` at the end of
    /// the action's lifecycle. A second concurrent `next()` is
    /// [`FfiError::ConcurrentNext`].
    pub async fn next(&self) -> Result<Option<FfiFollowActionStatus>, FfiError> {
        match &self.delivery {
            FollowActionDelivery::Receipt {
                receipt_id,
                statuses,
            } => match statuses.next().await {
                Ok(Some(status)) => Ok(Some(FfiFollowActionStatus::Receipt {
                    receipt_id: *receipt_id,
                    status: write_status_to_ffi(WriteStatusRef(&status)),
                })),
                Ok(None) => Ok(None),
                Err(nmp::FifoNextError::ConcurrentNext) => Err(FfiError::ConcurrentNext),
                Err(nmp::FifoNextError::Lagged) => Err(FfiError::FactStreamLagged {
                    receipt_id: Some(*receipt_id),
                }),
            },
            FollowActionDelivery::Immediate { statuses } => match statuses.next().await {
                Ok(status) => Ok(status),
                Err(nmp::FifoNextError::ConcurrentNext) => Err(FfiError::ConcurrentNext),
                Err(nmp::FifoNextError::Lagged) => {
                    Err(FfiError::FactStreamLagged { receipt_id: None })
                }
            },
        }
    }

    pub fn cancel(&self) {
        match &self.delivery {
            FollowActionDelivery::Receipt { statuses, .. } => statuses.close(),
            FollowActionDelivery::Immediate { statuses } => statuses.close(),
        }
    }
}

impl Drop for NmpFollowActionStream {
    fn drop(&mut self) {
        self.cancel();
    }
}

pub(crate) fn snapshot_to_ffi(snapshot: FollowSnapshot) -> FfiFollowSnapshot {
    FfiFollowSnapshot {
        current_pubkey: snapshot.current_pubkey.map(|pubkey| pubkey.to_hex()),
        target: snapshot.target.to_hex(),
        relationship: match snapshot.relationship {
            FollowRelationship::Unknown => FfiFollowRelationship::Unknown,
            FollowRelationship::NotFollowing => FfiFollowRelationship::NotFollowing,
            FollowRelationship::Following => FfiFollowRelationship::Following,
        },
        availability: match snapshot.availability {
            FollowAvailability::SignedOut => FfiFollowAvailability::SignedOut,
            FollowAvailability::Acquiring => FfiFollowAvailability::Acquiring,
            FollowAvailability::Ready => FfiFollowAvailability::Ready,
            FollowAvailability::NoContactList => FfiFollowAvailability::NoContactList,
            FollowAvailability::CachedOnly => FfiFollowAvailability::CachedOnly,
            FollowAvailability::SourceUnavailable => FfiFollowAvailability::SourceUnavailable,
        },
        base_event_id: snapshot.base_event_id.map(|id| id.to_hex()),
    }
}

pub(crate) fn failure_to_ffi(failure: FollowActionFailure) -> FfiFollowActionFailure {
    match failure {
        FollowActionFailure::SignedOut => FfiFollowActionFailure::SignedOut,
        FollowActionFailure::EngineClosed => FfiFollowActionFailure::EngineClosed,
        FollowActionFailure::ReceiptUnavailable => FfiFollowActionFailure::ReceiptUnavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::{EventId, Keys};

    #[test]
    fn snapshot_projection_is_lossless_for_relationship_state() {
        let active = Keys::generate().public_key();
        let target = Keys::generate().public_key();
        let base = EventId::all_zeros();
        let projected = snapshot_to_ffi(FollowSnapshot {
            current_pubkey: Some(active),
            target,
            relationship: FollowRelationship::Following,
            availability: FollowAvailability::Ready,
            base_event_id: Some(base),
        });
        assert_eq!(projected.current_pubkey, Some(active.to_hex()));
        assert_eq!(projected.target, target.to_hex());
        assert_eq!(projected.relationship, FfiFollowRelationship::Following);
        assert_eq!(projected.availability, FfiFollowAvailability::Ready);
        assert_eq!(projected.base_event_id, Some(base.to_hex()));
    }
}
