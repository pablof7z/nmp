//! Native projection of NMP's optional NIP-02 following resource/action.
//! This module only mirrors Rust-owned state and drains Rust-owned streams;
//! no contact-list parsing, replacement composition, readiness policy, or
//! optimistic following boolean lives at the FFI boundary.

use std::sync::Arc;

use crate::convert::{write_status_to_ffi, FfiError, WriteStatusRef};
use crate::types::FfiWriteStatus;
use nmp_nip02::{
    AsyncFollowObservation, ComposeFollowError, FollowActionFailure, FollowActionStatus,
    FollowAvailability, FollowRelationship, FollowSnapshot,
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
    pub active_pubkey: Option<String>,
    pub target: String,
    pub relationship: FfiFollowRelationship,
    pub availability: FfiFollowAvailability,
    pub base_event_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiFollowActionFailure {
    InvalidTarget { got: String },
    SignedOut,
    AccountChanged,
    AcquisitionTimedOut,
    NoContactList,
    CachedOnly,
    SourceUnavailable,
    BaseHasWrongAuthor,
    BaseHasWrongKind,
    TimestampExhausted,
    InvalidGeneratedTag,
    EngineClosed,
    ReceiptUnavailable,
    ThreadUnavailable { component: String, reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiFollowActionStatus {
    Acquiring,
    NoChange {
        following: bool,
    },
    Receipt {
        receipt_id: u64,
        status: FfiWriteStatus,
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

/// Pull-based follow/unfollow action stream (#680). The action worker (one
/// transient thread per user action on the engine's internal blocking-adapter
/// pool) pushes each [`FollowActionStatus`] into a waker-aware FIFO; this
/// handle awaits them in order. `None` is the terminal signal. `Drop`/`cancel`
/// close the stream.
#[derive(uniffi::Object)]
pub struct NmpFollowActionStream {
    inner: nmp::AsyncFifoReceiver<FollowActionStatus>,
}

impl NmpFollowActionStream {
    pub(crate) fn new(inner: nmp::AsyncFifoReceiver<FollowActionStatus>) -> Arc<Self> {
        Arc::new(Self { inner })
    }
}

#[uniffi::export]
impl NmpFollowActionStream {
    /// Await the next follow-action status in order, or `None` at the end of
    /// the action's lifecycle. A second concurrent `next()` is
    /// [`FfiError::ConcurrentNext`].
    pub async fn next(&self) -> Result<Option<FfiFollowActionStatus>, FfiError> {
        match self.inner.next().await {
            Ok(Some(status)) => Ok(Some(action_status_to_ffi(status))),
            Ok(None) => Ok(None),
            Err(_) => Err(FfiError::ConcurrentNext),
        }
    }

    pub fn cancel(&self) {
        self.inner.close();
    }
}

impl Drop for NmpFollowActionStream {
    fn drop(&mut self) {
        self.inner.close();
    }
}

pub(crate) fn snapshot_to_ffi(snapshot: FollowSnapshot) -> FfiFollowSnapshot {
    FfiFollowSnapshot {
        active_pubkey: snapshot.active_pubkey.map(|pubkey| pubkey.to_hex()),
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

fn failure_to_ffi(failure: FollowActionFailure) -> FfiFollowActionFailure {
    match failure {
        FollowActionFailure::InvalidTarget { got } => FfiFollowActionFailure::InvalidTarget { got },
        FollowActionFailure::SignedOut => FfiFollowActionFailure::SignedOut,
        FollowActionFailure::AccountChanged => FfiFollowActionFailure::AccountChanged,
        FollowActionFailure::AcquisitionTimedOut => FfiFollowActionFailure::AcquisitionTimedOut,
        FollowActionFailure::NoContactList => FfiFollowActionFailure::NoContactList,
        FollowActionFailure::CachedOnly => FfiFollowActionFailure::CachedOnly,
        FollowActionFailure::SourceUnavailable => FfiFollowActionFailure::SourceUnavailable,
        FollowActionFailure::Compose(error) => match error {
            ComposeFollowError::BaseHasWrongAuthor => FfiFollowActionFailure::BaseHasWrongAuthor,
            ComposeFollowError::BaseHasWrongKind => FfiFollowActionFailure::BaseHasWrongKind,
            ComposeFollowError::TimestampExhausted => FfiFollowActionFailure::TimestampExhausted,
            ComposeFollowError::InvalidGeneratedTag => FfiFollowActionFailure::InvalidGeneratedTag,
        },
        FollowActionFailure::EngineClosed => FfiFollowActionFailure::EngineClosed,
        FollowActionFailure::ReceiptUnavailable => FfiFollowActionFailure::ReceiptUnavailable,
        FollowActionFailure::ThreadUnavailable { component, reason } => {
            FfiFollowActionFailure::ThreadUnavailable { component, reason }
        }
    }
}

pub(crate) fn action_status_to_ffi(status: FollowActionStatus) -> FfiFollowActionStatus {
    match status {
        FollowActionStatus::Acquiring => FfiFollowActionStatus::Acquiring,
        FollowActionStatus::NoChange { following } => FfiFollowActionStatus::NoChange { following },
        FollowActionStatus::Receipt { receipt_id, status } => FfiFollowActionStatus::Receipt {
            receipt_id,
            status: write_status_to_ffi(WriteStatusRef(&status)),
        },
        FollowActionStatus::Failed(failure) => FfiFollowActionStatus::Failed {
            failure: failure_to_ffi(failure),
        },
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
            active_pubkey: Some(active),
            target,
            relationship: FollowRelationship::Following,
            availability: FollowAvailability::Ready,
            base_event_id: Some(base),
        });
        assert_eq!(projected.active_pubkey, Some(active.to_hex()));
        assert_eq!(projected.target, target.to_hex());
        assert_eq!(projected.relationship, FfiFollowRelationship::Following);
        assert_eq!(projected.availability, FfiFollowAvailability::Ready);
        assert_eq!(projected.base_event_id, Some(base.to_hex()));
    }
}
