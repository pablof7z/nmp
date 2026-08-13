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

#[cfg(test)]
mod tests {
    use nmp_store::{IntentId, MaterializationId, MaterializationRef};
    use nostr::EventId;

    use super::MaterializationDeliveryOwner;

    fn event_id(byte: u8) -> EventId {
        EventId::from_byte_array([byte; 32])
    }

    #[test]
    fn one_materialization_selects_one_physical_owner_for_every_member() {
        let materialization = MaterializationRef {
            materialization_id: MaterializationId(9),
            event_id: event_id(7),
        };

        let owner = MaterializationDeliveryOwner::new(
            materialization,
            [IntentId(13), IntentId(5), IntentId(8)],
        )
        .expect("a nonempty generation has a delivery owner");

        assert_eq!(owner.materialization(), materialization);
        assert_eq!(owner.physical_owner(), IntentId(5));
        assert_eq!(
            owner.members().copied().collect::<Vec<_>>(),
            vec![IntentId(5), IntentId(8), IntentId(13)]
        );
    }

    #[test]
    fn an_empty_member_set_cannot_create_unowned_delivery_work() {
        let materialization = MaterializationRef {
            materialization_id: MaterializationId(1),
            event_id: event_id(3),
        };

        assert!(MaterializationDeliveryOwner::new(materialization, []).is_none());
    }
}
