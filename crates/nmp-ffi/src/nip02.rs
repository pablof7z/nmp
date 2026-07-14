//! Native projection of NMP's optional NIP-02 following resource/action.
//! This module only mirrors Rust-owned state and drains Rust-owned streams;
//! no contact-list parsing, replacement composition, readiness policy, or
//! optimistic following boolean lives at the FFI boundary.

use std::sync::Arc;

use crate::convert::{write_status_to_ffi, WriteStatusRef};
use crate::types::FfiWriteStatus;
use nmp_nip02::{
    ComposeFollowError, FollowActionFailure, FollowActionStatus, FollowAvailability,
    FollowRelationship, FollowSnapshot,
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

#[uniffi::export(callback_interface)]
pub trait FollowObserver: Send + Sync {
    fn on_snapshot(&self, snapshot: FfiFollowSnapshot);
    fn on_closed(&self);
}

#[uniffi::export(callback_interface)]
pub trait FollowActionObserver: Send + Sync {
    fn on_status(&self, status: FfiFollowActionStatus);
    fn on_closed(&self);
}

/// Deinit-tied cancellation for one following relationship observation.
#[derive(uniffi::Object)]
pub struct NmpFollowHandle {
    pub(crate) cancel: nmp::ObservationCancel,
}

#[uniffi::export]
impl NmpFollowHandle {
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

impl Drop for NmpFollowHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
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

pub(crate) fn handle(cancel: nmp::ObservationCancel) -> Arc<NmpFollowHandle> {
    Arc::new(NmpFollowHandle { cancel })
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
