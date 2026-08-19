use std::collections::BTreeSet;

use nmp_store::{IntentId, MaterializationRef};

/// The one ordinary-publisher owner for a complete semantic materialization.
///
/// Operation receipts remain distinct app-facing lifecycles, but their shared
/// current event must not multiply physical signer or relay work. Selecting
/// the smallest contributing durable intent makes ownership deterministic on
/// acceptance, restart, and every successor installation without persisting a
/// second identity beside the materialization's existing member set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MaterializationDeliveryOwner {
    materialization: MaterializationRef,
    physical_owner: IntentId,
    members: BTreeSet<IntentId>,
}

impl MaterializationDeliveryOwner {
    pub(super) fn new(
        materialization: MaterializationRef,
        members: impl IntoIterator<Item = IntentId>,
    ) -> Option<Self> {
        let members = members.into_iter().collect::<BTreeSet<_>>();
        let physical_owner = members.first().copied()?;
        Some(Self {
            materialization,
            physical_owner,
            members,
        })
    }

    pub(super) fn materialization(&self) -> MaterializationRef {
        self.materialization
    }

    pub(super) fn physical_owner(&self) -> IntentId {
        self.physical_owner
    }

    pub(super) fn members(&self) -> impl Iterator<Item = &IntentId> {
        self.members.iter()
    }
}

