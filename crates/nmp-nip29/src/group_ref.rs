//! Typed remembered-group/host composition over `nmp_nip51`'s decoded
//! kind:10009 output (#63/#108). A PURE mapping -- no second subscription,
//! no re-acquisition; the SAME kind:10009 `Demand` `nmp-nip51` already
//! declares is the only read involved.

use nostr::RelayUrl;

use nmp_nip51::SimpleGroupsList;

/// A remembered NIP-29 group reference -- exactly the fields VISION.md's
/// own composition example names: "group references containing a group id
/// and host relay" (plus the optional display name #63's `SimpleGroupEntry`
/// already carries). Thin NIP-29-facing reshaping of
/// [`nmp_nip51::SimpleGroupEntry`] -- `nmp-nip29` never claims kind:10009,
/// it only re-labels the already-decoded value (#63's ownership boundary).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupRef {
    pub group_id: String,
    pub host: RelayUrl,
    pub name: Option<String>,
}

/// The composed remembered-groups/host-relays value a `nmp-nip29` consumer
/// gets (#63: "nmp-nip29 exposes a typed remembered-groups/host-relays
/// value by composing those entries"). `groups` preserves the source
/// list's exact order; `hosts_in_use` is the list's own `r`-tagged relay
/// set, distinct from any individual group's `host`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RememberedGroups {
    pub groups: Vec<GroupRef>,
    pub hosts_in_use: Vec<RelayUrl>,
    /// Carried through from `SimpleGroupsList` unchanged -- see that
    /// type's own doc for why this is evidence, not a silent drop.
    pub has_private_content: bool,
}

/// Compose an already-decoded [`SimpleGroupsList`] into the NIP-29-facing
/// [`RememberedGroups`] value. Pure: no I/O, no acquisition -- `list` is
/// whatever `nmp-nip51`'s own kind:10009 read already produced.
pub fn remembered_groups(list: &SimpleGroupsList) -> RememberedGroups {
    RememberedGroups {
        groups: list
            .items
            .iter()
            .map(|entry| GroupRef {
                group_id: entry.group_id.clone(),
                host: entry.host_relay.clone(),
                name: entry.name.clone(),
            })
            .collect(),
        hosts_in_use: list.relays_in_use.clone(),
        has_private_content: list.has_private_content,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nmp_nip51::SimpleGroupEntry;

    #[test]
    fn composes_entries_into_group_refs_preserving_order() {
        let list = SimpleGroupsList {
            items: vec![
                SimpleGroupEntry {
                    group_id: "group-a".to_string(),
                    host_relay: RelayUrl::parse("wss://relay-a.example.com").unwrap(),
                    name: Some("Group A".to_string()),
                },
                SimpleGroupEntry {
                    group_id: "group-b".to_string(),
                    host_relay: RelayUrl::parse("wss://relay-b.example.com").unwrap(),
                    name: None,
                },
            ],
            relays_in_use: vec![RelayUrl::parse("wss://relay-c.example.com").unwrap()],
            malformed_item_count: 0,
            has_private_content: true,
        };

        let remembered = remembered_groups(&list);
        assert_eq!(remembered.groups.len(), 2);
        assert_eq!(remembered.groups[0].group_id, "group-a");
        assert_eq!(remembered.groups[0].name.as_deref(), Some("Group A"));
        assert_eq!(remembered.groups[1].group_id, "group-b");
        assert_eq!(remembered.groups[1].name, None);
        assert_eq!(
            remembered.hosts_in_use,
            vec![RelayUrl::parse("wss://relay-c.example.com").unwrap()]
        );
        assert!(remembered.has_private_content);
    }
}
