//! NIP-51 Simple groups: tolerant, observational parsing at the FFI
//! boundary (#863).
//!
//! An [`FfiRow`] crossing this boundary is CALLER-CONSTRUCTIBLE -- a native
//! app can invent every field, including `kind`, `sig`, and `sources`. So
//! the only honest thing this module can offer is a tolerant reader whose
//! result is plain data: `parse_simple_groups_list_tolerant` names that in
//! the API itself, and [`FfiSimpleGroupsList`] documents it in the type.
//!
//! Deliberately absent, and mechanically kept absent by
//! `scripts/check-nip51-no-derived-authority.sh`: any observation-qualified
//! `Observed*` wrapper, projection-error family, frame-proof projector, or
//! other protocol-specific witness. NIP-51 reading stays the ordinary
//! `LiveQuery`/`FfiDemand` noun (`crate::nip29::active_account_demand`), and
//! a future destructive NIP-51 mutation must bind its exact observed base
//! privately inside that semantic operation while building the ordinary
//! opaque write intent -- never by exporting a reusable authority noun here.

use nostr::RelayUrl;

use crate::types::{FfiRow, FfiSimpleGroupEntry, FfiSimpleGroupsList};

fn simple_group_entry_to_ffi(entry: &nmp_nip51::SimpleGroupEntry) -> FfiSimpleGroupEntry {
    FfiSimpleGroupEntry {
        group_id: entry.group_id.clone(),
        host_relay: entry.host_relay.to_string(),
        name: entry.name.clone(),
    }
}

fn simple_groups_list_to_ffi(list: &nmp_nip51::SimpleGroupsList) -> FfiSimpleGroupsList {
    FfiSimpleGroupsList {
        items: list.items.iter().map(simple_group_entry_to_ffi).collect(),
        relays_in_use: list.relays_in_use.iter().map(RelayUrl::to_string).collect(),
        malformed_item_count: u64::try_from(list.malformed_item_count)
            .expect("usize always fits u64 on supported FFI targets"),
        has_private_content: list.has_private_content,
    }
}

/// Tolerantly parse Simple-groups-shaped public items out of a raw native
/// row (#863). Infallible, and deliberately kind-agnostic: `row` may carry
/// any `kind`, an invented `sig`, and no `sources` at all.
///
/// The result preserves malformed-item and private-content evidence, and
/// grants NO signature, canonical-store, provenance, routing, or mutation
/// authority. To browse a NIP-29 group the app still passes an explicit host
/// of its own choosing to `group_discovery_demand`/`group_content_demand`;
/// nothing here authorizes a host on the app's behalf.
#[uniffi::export]
pub fn parse_simple_groups_list_tolerant(row: FfiRow) -> FfiSimpleGroupsList {
    simple_groups_list_to_ffi(&nmp_nip51::parse_simple_groups_list_from_raw_tags_tolerant(
        row.tags.iter().map(|tag| tag.as_slice()),
        &row.content,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fabricated_row(kind: u16) -> FfiRow {
        FfiRow {
            id: "caller-chosen-id".to_owned(),
            pubkey: "caller-chosen-pubkey".to_owned(),
            created_at: 1,
            kind,
            tags: vec![
                vec![
                    "group".to_owned(),
                    "group-a".to_owned(),
                    "wss://relay-a.example.com".to_owned(),
                    "Group A".to_owned(),
                ],
                vec!["group".to_owned(), "missing-relay".to_owned()],
                vec!["r".to_owned(), "wss://relay-in-use.example.com".to_owned()],
            ],
            content: "encrypted-private-items".to_owned(),
            sig: "caller-chosen-signature".to_owned(),
            sources: vec![],
        }
    }

    /// #863's FFI falsifier: a row the caller fabricated -- wrong kind,
    /// invented signature, no relay sources -- still parses, still reports
    /// its malformed/private evidence, and still yields nothing but data.
    #[test]
    fn tolerant_parser_preserves_evidence_even_for_fabricated_wrong_kind_row() {
        let list = parse_simple_groups_list_tolerant(fabricated_row(1));
        assert_eq!(list.items.len(), 1);
        assert_eq!(list.items[0].group_id, "group-a");
        assert_eq!(list.items[0].host_relay, "wss://relay-a.example.com");
        assert_eq!(list.items[0].name.as_deref(), Some("Group A"));
        assert_eq!(list.relays_in_use, vec!["wss://relay-in-use.example.com"]);
        assert_eq!(list.malformed_item_count, 1);
        assert!(list.has_private_content);

        // The kind:10009 spelling buys the value nothing extra: identical
        // input parses identically, so no consumer can read provenance,
        // canonicality, or routing permission out of the result.
        assert_eq!(
            parse_simple_groups_list_tolerant(fabricated_row(10_009)),
            list
        );
    }

    /// The host an app browses with is its own explicit typed input, never
    /// harvested from parser output by the boundary itself.
    #[test]
    fn nip29_browsing_still_demands_an_explicitly_supplied_host() {
        let list = parse_simple_groups_list_tolerant(fabricated_row(10_009));
        let chosen = list.items[0].host_relay.clone();
        let demand =
            crate::nip29::group_discovery_demand(chosen).expect("app-supplied host parses");
        assert_eq!(demand.selection.kinds, Some(vec![39000]));
    }
}
