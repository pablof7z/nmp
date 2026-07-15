//! Native projection of NMP's guarded kind:10009 relay-list action. This
//! mirrors Rust-owned action state only; native callers never receive raw tags,
//! choose a base, or manufacture a replacement event.

use crate::convert::{write_status_to_ffi, WriteStatusRef};
use crate::types::FfiWriteStatus;
use nmp_nip51::{ComposeRelayChangeError, RelayActionFailure, RelayActionStatus};

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiRelayListActionFailure {
    InvalidRelay { got: String },
    SignedOut,
    AccountChanged,
    AcquisitionTimedOut,
    CachedOnly,
    SourceUnavailable,
    BaseHasWrongAuthor,
    BaseHasWrongKind,
    TimestampExhausted,
    InvalidGeneratedTag,
    EngineClosed,
    ReceiptUnavailable,
    ThreadUnavailable { component: String, reason: String },
    ExecutorSaturated { component: String, capacity: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiRelayListActionStatus {
    Acquiring,
    NoChange {
        present: bool,
    },
    Receipt {
        receipt_id: u64,
        status: FfiWriteStatus,
    },
    Failed {
        failure: FfiRelayListActionFailure,
    },
}

#[uniffi::export(callback_interface)]
pub trait RelayListActionObserver: Send + Sync {
    fn on_status(&self, status: FfiRelayListActionStatus);
    fn on_closed(&self);
}

fn failure_to_ffi(failure: RelayActionFailure) -> FfiRelayListActionFailure {
    match failure {
        RelayActionFailure::SignedOut => FfiRelayListActionFailure::SignedOut,
        RelayActionFailure::AccountChanged => FfiRelayListActionFailure::AccountChanged,
        RelayActionFailure::AcquisitionTimedOut => FfiRelayListActionFailure::AcquisitionTimedOut,
        RelayActionFailure::CachedOnly => FfiRelayListActionFailure::CachedOnly,
        RelayActionFailure::SourceUnavailable => FfiRelayListActionFailure::SourceUnavailable,
        RelayActionFailure::Compose(error) => match error {
            ComposeRelayChangeError::BaseHasWrongAuthor => {
                FfiRelayListActionFailure::BaseHasWrongAuthor
            }
            ComposeRelayChangeError::BaseHasWrongKind => {
                FfiRelayListActionFailure::BaseHasWrongKind
            }
            ComposeRelayChangeError::TimestampExhausted => {
                FfiRelayListActionFailure::TimestampExhausted
            }
            ComposeRelayChangeError::InvalidGeneratedTag => {
                FfiRelayListActionFailure::InvalidGeneratedTag
            }
        },
        RelayActionFailure::EngineClosed => FfiRelayListActionFailure::EngineClosed,
        RelayActionFailure::ReceiptUnavailable => FfiRelayListActionFailure::ReceiptUnavailable,
        RelayActionFailure::ThreadUnavailable { component, reason } => {
            FfiRelayListActionFailure::ThreadUnavailable { component, reason }
        }
        RelayActionFailure::ExecutorSaturated {
            component,
            capacity,
        } => FfiRelayListActionFailure::ExecutorSaturated {
            component,
            capacity: capacity as u64,
        },
    }
}

pub(crate) fn action_status_to_ffi(status: RelayActionStatus) -> FfiRelayListActionStatus {
    match status {
        RelayActionStatus::Acquiring => FfiRelayListActionStatus::Acquiring,
        RelayActionStatus::NoChange { present } => FfiRelayListActionStatus::NoChange { present },
        RelayActionStatus::Receipt { receipt_id, status } => FfiRelayListActionStatus::Receipt {
            receipt_id,
            status: write_status_to_ffi(WriteStatusRef(&status)),
        },
        RelayActionStatus::Failed(failure) => FfiRelayListActionStatus::Failed {
            failure: failure_to_ffi(failure),
        },
    }
}
