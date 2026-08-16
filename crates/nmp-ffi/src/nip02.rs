//! Native projection of NMP's optional NIP-02 following resource, and the
//! typed follow/unfollow action's pre-custody refusal (#1640). The typed
//! current-following read stays a projection over the ordinary live-query
//! path ([`NmpFollowStream`]); the write side returns the ordinary
//! [`crate::facade::NmpReceiptStream`] directly -- no follow-only
//! action/status stream, registration handle, or second cancellation
//! lifecycle exists at this boundary.

use std::sync::Arc;

use crate::convert::FfiError;
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

/// Why a typed follow/unfollow action was refused before ordinary receipt
/// custody. `InvalidTarget` is the one refusal this boundary adds to
/// [`nmp_nip02::FollowActionFailure`]: `target` crosses FFI as a
/// caller-typed hex string, which the Rust-native API never has to parse.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum FfiFollowActionError {
    InvalidTarget { got: String },
    AutomaticRoutingUnavailable,
    SignedOut,
    EngineClosed,
    PublishRefused { reason: String },
}

impl std::fmt::Display for FfiFollowActionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidTarget { got } => write!(f, "invalid public key: {got}"),
            Self::AutomaticRoutingUnavailable => {
                f.write_str("automatic author/outbox routing is not configured")
            }
            Self::SignedOut => f.write_str("no current account is selected"),
            Self::EngineClosed => f.write_str("the engine is closed"),
            Self::PublishRefused { reason } => write!(f, "{reason}"),
        }
    }
}

impl std::error::Error for FfiFollowActionError {}

impl From<FollowActionFailure> for FfiFollowActionError {
    fn from(failure: FollowActionFailure) -> Self {
        match failure {
            FollowActionFailure::SignedOut => Self::SignedOut,
            FollowActionFailure::EngineClosed => Self::EngineClosed,
            FollowActionFailure::PublishRefused { reason } => Self::PublishRefused { reason },
        }
    }
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
