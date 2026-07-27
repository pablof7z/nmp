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
//! `LiveQuery`/`FfiDemand` noun ([`active_account_demand`]), and a future
//! destructive NIP-51 mutation must bind its exact observed base privately
//! inside that semantic operation while building the ordinary opaque write
//! intent -- never by exporting a reusable authority noun here.
//!
//! Also deliberately absent since #858: any second projection of this value.
//! [`FfiSimpleGroupsList`] is the ONE native shape a decoded kind:10009 list
//! takes. The NIP-29-facing wrapper family that used to sit beside it merely
//! renamed these fields and dropped `malformed_item_count` -- exactly the
//! second-owner shape #63's boundary exists to forbid. A caller that wants to
//! browse a group picks one [`FfiSimpleGroupEntry`] and passes its
//! `host_relay`/`group_id` to `crate::nip29`'s constructors itself.

use nostr::RelayUrl;

use crate::convert::demand_to_ffi;
use crate::types::{FfiDemand, FfiRow, FfiSimpleGroupEntry, FfiSimpleGroupsList};

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

/// The signed-in account's Simple-groups-list demand (#108,
/// `nmp_nip51::active_account_demand` mirror): `kinds:[10009]`,
/// `AuthorOutboxes + Public`. Signed-out (no active account) resolves to
/// zero atoms through the ordinary reactive-binding empty-resolution path
/// -- no special case needed on either side of this boundary.
///
/// #858 moved this out of `crate::nip29`: kind:10009 is NIP-51's kind, so
/// its demand constructor lives with the rest of NIP-51.
#[uniffi::export]
pub fn active_account_demand() -> FfiDemand {
    demand_to_ffi(nmp_nip51::active_account_demand())
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

    #[test]
    fn active_account_demand_projects_the_reactive_authors_binding() {
        let demand = active_account_demand();
        assert_eq!(demand.selection.kinds, Some(vec![10009]));
    }

    /// The host an app browses with is its own explicit typed input, never
    /// harvested from parser output by the boundary itself.
    ///
    /// #858's FFI falsifier too: the SELECTED entry feeds NIP-29's
    /// host-pinned constructors directly, field for field, with no
    /// intermediate NIP-29-owned copy of the NIP-51 value in between.
    #[test]
    fn nip29_browsing_still_demands_an_explicitly_supplied_host() {
        let list = parse_simple_groups_list_tolerant(fabricated_row(10_009));
        let selected = list.items[0].clone();
        let demand = crate::nip29::group_discovery_demand(selected.host_relay.clone())
            .expect("app-supplied host parses");
        assert_eq!(demand.selection.kinds, Some(vec![39000]));

        let content = crate::nip29::group_content_demand(selected.host_relay, selected.group_id)
            .expect("app-supplied host parses");
        assert_eq!(content.selection.kinds, Some(vec![9, 30315]));
        assert_eq!(content.source, demand.source);
    }
}
