//! UniFFI projection of engine-free reference targets and demand plans.

use nmp_grammar::reference::{ReferencePlanError, ReferenceTarget};
use uniffi::{Enum, Record};

use crate::convert::{demand_to_ffi, FfiError};
use crate::types::FfiDemand;

#[derive(Debug, Clone, PartialEq, Eq, Enum)]
pub enum FfiReferenceTarget {
    Profile {
        pubkey: String,
        relay_hints: Vec<String>,
    },
    Event {
        id: String,
        author_hint: Option<String>,
        kind_hint: Option<u16>,
        relay_hints: Vec<String>,
    },
    Address {
        kind: u16,
        author: String,
        identifier: String,
        relay_hints: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiReferenceDemandPlan {
    pub target_key: String,
    pub canonical: FfiDemand,
    pub helpers: Vec<FfiDemand>,
    pub discarded_relay_hints: u32,
}

#[uniffi::export]
pub fn reference_demand_plan(
    target: FfiReferenceTarget,
) -> Result<FfiReferenceDemandPlan, FfiError> {
    let plan = target_from_ffi(target)
        .demand_plan()
        .map_err(reference_plan_error_to_ffi)?;
    Ok(FfiReferenceDemandPlan {
        target_key: plan.target_key,
        canonical: demand_to_ffi(plan.canonical),
        helpers: plan.helpers.into_iter().map(demand_to_ffi).collect(),
        discarded_relay_hints: plan.discarded_relay_hints,
    })
}

fn reference_plan_error_to_ffi(error: ReferencePlanError) -> FfiError {
    match error {
        ReferencePlanError::InvalidProfilePublicKey { got }
        | ReferencePlanError::InvalidAddressAuthor { got } => FfiError::InvalidPublicKey { got },
        ReferencePlanError::InvalidEventId { got } => FfiError::InvalidEventId { got },
    }
}

pub(crate) fn target_to_ffi(value: ReferenceTarget) -> FfiReferenceTarget {
    match value {
        ReferenceTarget::Profile {
            pubkey,
            relay_hints,
        } => FfiReferenceTarget::Profile {
            pubkey,
            relay_hints,
        },
        ReferenceTarget::Event {
            id,
            author_hint,
            kind_hint,
            relay_hints,
        } => FfiReferenceTarget::Event {
            id,
            author_hint,
            kind_hint,
            relay_hints,
        },
        ReferenceTarget::Address {
            kind,
            author,
            identifier,
            relay_hints,
        } => FfiReferenceTarget::Address {
            kind,
            author,
            identifier,
            relay_hints,
        },
    }
}

fn target_from_ffi(value: FfiReferenceTarget) -> ReferenceTarget {
    match value {
        FfiReferenceTarget::Profile {
            pubkey,
            relay_hints,
        } => ReferenceTarget::Profile {
            pubkey,
            relay_hints,
        },
        FfiReferenceTarget::Event {
            id,
            author_hint,
            kind_hint,
            relay_hints,
        } => ReferenceTarget::Event {
            id,
            author_hint,
            kind_hint,
            relay_hints,
        },
        FfiReferenceTarget::Address {
            kind,
            author,
            identifier,
            relay_hints,
        } => ReferenceTarget::Address {
            kind,
            author,
            identifier,
            relay_hints,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_target_is_refused_before_it_can_become_a_demand() {
        assert!(matches!(
            reference_demand_plan(FfiReferenceTarget::Event {
                id: "not-an-id".to_string(),
                author_hint: None,
                kind_hint: None,
                relay_hints: Vec::new(),
            }),
            Err(FfiError::InvalidEventId { .. })
        ));
    }
}
