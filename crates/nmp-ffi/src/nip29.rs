//! Read-only NIP-29 host-browser projection (#108): top-level free
//! functions, same shape as [`crate::entity`]'s precedent (#116) -- these
//! need no `NmpEngine` instance, only `nmp-nip51`/`nmp-nip29` directly
//! (deliberately NOT proxied through the `nmp` facade -- see those
//! crates' own Cargo.toml comments: they stay opt-in, app-chosen
//! dependencies at the direct-Rust layer, #63/#45's "opt-in protocol
//! crate" framing; `nmp-ffi` bundles the projection because it is the ONE
//! staticlib/cdylib every Swift/Kotlin app links, not because these
//! crates became part of the canonical facade).
//!
//! The selected host rides as `SourceAuthority::Pinned({host})` on the
//! returned `FfiDemand` -- pass it straight to
//! `NmpEngine::observe_demand`, exactly like any other `FfiDemand` (#107).
//! No new subscribe verb exists or is needed for this feature.

use nostr::RelayUrl;

use crate::convert::{demand_to_ffi, FfiError};
use crate::types::{FfiDemand, FfiGroupRef, FfiRememberedGroups, FfiRow};

fn group_ref_to_ffi(g: nmp_nip29::GroupRef) -> FfiGroupRef {
    FfiGroupRef {
        group_id: g.group_id,
        host: g.host.to_string(),
        name: g.name,
    }
}

fn parse_host(host: String) -> Result<RelayUrl, FfiError> {
    RelayUrl::parse(&host).map_err(|_| FfiError::InvalidRelayUrl { got: host })
}

/// The signed-in account's remembered-groups demand (#108,
/// `nmp_nip51::active_account_demand` mirror): `kinds:[10009]`,
/// `AuthorOutboxes + Public`. Signed-out (no active account) resolves to
/// zero atoms through the ordinary reactive-binding empty-resolution path
/// -- no special case needed on either side of this boundary.
#[uniffi::export]
pub fn active_account_demand() -> FfiDemand {
    demand_to_ffi(nmp_nip51::active_account_demand())
}

/// Group discovery (kind:39000) pinned to `host` (#108,
/// `nmp_nip29::group_discovery_demand` mirror). `host` crosses the FFI
/// boundary as a raw string, unlike the direct-Rust constructor's
/// `RelayUrl` -- fallibility is restored HERE (an FFI caller can supply a
/// malformed URL the direct-Rust singleton-set proof doesn't cover).
#[uniffi::export]
pub fn group_discovery_demand(host: String) -> Result<FfiDemand, FfiError> {
    Ok(demand_to_ffi(nmp_nip29::group_discovery_demand(
        parse_host(host)?,
    )))
}

/// Group content (kinds 9, 30315), `h`-tag scoped to `group_id`, pinned to
/// `host` (#108, `nmp_nip29::group_content_demand` mirror).
#[uniffi::export]
pub fn group_content_demand(host: String, group_id: String) -> Result<FfiDemand, FfiError> {
    Ok(demand_to_ffi(nmp_nip29::group_content_demand(
        parse_host(host)?,
        &group_id,
    )))
}

/// Decode a delivered kind:10009 [`FfiRow`] into the composed remembered-
/// groups/host-relays value (#108). Infallible, mirroring
/// `nmp_nip51::decode_simple_groups_list`'s own never-fails contract:
/// malformed individual items are dropped, never the whole decode.
#[uniffi::export]
pub fn decode_remembered_groups(row: FfiRow) -> FfiRememberedGroups {
    let list = nmp_nip51::decode_simple_groups_list_from_raw_tags(
        row.tags.iter().map(|t| t.as_slice()),
        &row.content,
    );
    let remembered = nmp_nip29::remembered_groups(&list);
    FfiRememberedGroups {
        groups: remembered
            .groups
            .into_iter()
            .map(group_ref_to_ffi)
            .collect(),
        hosts_in_use: remembered
            .hosts_in_use
            .iter()
            .map(RelayUrl::to_string)
            .collect(),
        has_private_content: remembered.has_private_content,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FfiSourceAuthority;

    #[test]
    fn active_account_demand_projects_the_reactive_authors_binding() {
        let demand = active_account_demand();
        assert_eq!(demand.selection.kinds, Some(vec![10009]));
    }

    #[test]
    fn group_discovery_demand_pins_a_parsed_host() {
        let demand = group_discovery_demand("wss://host-1.example.com".to_string())
            .expect("well-formed host url");
        assert_eq!(demand.selection.kinds, Some(vec![39000]));
        match demand.source {
            FfiSourceAuthority::Pinned { relays } => {
                assert_eq!(relays, vec!["wss://host-1.example.com".to_string()]);
            }
            other => panic!("expected Pinned, got {other:?}"),
        }
    }

    #[test]
    fn group_discovery_demand_rejects_an_unparseable_host() {
        match group_discovery_demand("not-a-url".to_string()) {
            Err(FfiError::InvalidRelayUrl { got }) => assert_eq!(got, "not-a-url"),
            other => panic!("expected InvalidRelayUrl, got {other:?}"),
        }
    }

    #[test]
    fn group_content_demand_scopes_by_h_tag() {
        let demand = group_content_demand(
            "wss://host-1.example.com".to_string(),
            "group-a".to_string(),
        )
        .expect("well-formed host url");
        assert_eq!(demand.selection.kinds, Some(vec![9, 30315]));
    }

    #[test]
    fn decode_remembered_groups_composes_a_kind_10009_row() {
        let row = FfiRow {
            id: "id".to_string(),
            pubkey: "pubkey".to_string(),
            created_at: 1,
            kind: 10009,
            tags: vec![vec![
                "group".to_string(),
                "group-a".to_string(),
                "wss://relay-a.example.com".to_string(),
                "Group A".to_string(),
            ]],
            content: String::new(),
            sig: "sig".to_string(),
            sources: vec![],
        };
        let remembered = decode_remembered_groups(row);
        assert_eq!(remembered.groups.len(), 1);
        assert_eq!(remembered.groups[0].group_id, "group-a");
        assert_eq!(remembered.groups[0].host, "wss://relay-a.example.com");
        assert_eq!(remembered.groups[0].name.as_deref(), Some("Group A"));
        assert!(!remembered.has_private_content);
    }
}
