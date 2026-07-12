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
//!
//! `group_send_intent`/[`FfiComposedWriteIntent`] (#115) are this module's
//! write-side counterpart: an app couriers rows it already has from a live
//! `group_content_demand` read, and this crate's `nmp_nip29::
//! compose_group_send` owns 100% of the `h`/`previous` tag composition --
//! the app never sees either tag, `WriteRouting`, or `HostAuthority`
//! directly, only the opaque, take-once handle `NmpEngine::
//! publish_composed` consumes.

use std::sync::{Arc, Mutex};

use nostr::{EventId, RelayUrl, Timestamp};

use crate::convert::{demand_to_ffi, parse_pubkey, FfiError};
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

/// Take-once wrapper around a `nmp_nip29::compose_group_send`-composed
/// `WriteIntent` (#115). Opaque and generically named -- a future protocol
/// module's own composed intent could reuse this same wrapper shape,
/// nothing here is NIP-29-specific except how it's constructed.
/// Take-once, not `Clone`/re-readable: [`NmpEngine::publish_composed`]
/// (`crate::facade`) takes the inner intent exactly once, and a second
/// call fails closed with [`FfiError::IntentAlreadyConsumed`] rather than
/// silently re-publishing a stale template or handing back nothing.
#[derive(uniffi::Object)]
pub struct FfiComposedWriteIntent {
    inner: Mutex<Option<nmp_grammar::WriteIntent>>,
}

impl FfiComposedWriteIntent {
    /// Take the wrapped intent exactly once. Called only from
    /// `crate::facade::NmpEngine::publish_composed`.
    pub(crate) fn take(&self) -> Result<nmp_grammar::WriteIntent, FfiError> {
        self.inner
            .lock()
            .expect("FfiComposedWriteIntent mutex poisoned")
            .take()
            .ok_or(FfiError::IntentAlreadyConsumed)
    }
}

/// Compose a NIP-29 group send (#115): `recent_rows` are delivered
/// kind:9/30315 rows the app is already rendering from its own live
/// `group_content_demand` read (#108) -- couriered, not hand-rolled (see
/// `nmp_nip29::compose_group_send`'s own doc for that distinction). This
/// function owns 100% of the `h`/`previous` tag
/// selection/verification/truncation/encoding; the app supplies only the
/// primitives it already has.
///
/// `kind` is entirely the caller's choice -- this function (and everything
/// it calls) is kind-blind. Publish the result via
/// [`crate::facade::NmpEngine::publish_composed`].
// Mirrors `nmp_nip29::compose_group_send`'s own ratified 8-argument
// signature one-for-one across the FFI boundary (plus `recent_rows` in
// place of a `&GroupTimelineEvidence` reference); same
// `#[allow(clippy::too_many_arguments)]` precedent as that function.
#[allow(clippy::too_many_arguments)]
#[uniffi::export]
pub fn group_send_intent(
    host: String,
    group_id: String,
    author_pubkey: String,
    created_at: u64,
    kind: u16,
    content: String,
    extra_tags: Vec<Vec<String>>,
    recent_rows: Vec<FfiRow>,
) -> Result<Arc<FfiComposedWriteIntent>, FfiError> {
    let host = parse_host(host)?;
    let author = parse_pubkey(&author_pubkey)?;

    let rows = recent_rows
        .into_iter()
        .map(|row| {
            let id = EventId::from_hex(&row.id).map_err(|_| FfiError::InvalidEventId {
                got: row.id.clone(),
            })?;
            Ok((id, row.created_at, row.tags))
        })
        .collect::<Result<Vec<_>, FfiError>>()?;
    let previous = nmp_nip29::GroupTimelineEvidence::from_events(&group_id, rows);

    let intent = nmp_nip29::compose_group_send(
        host,
        &group_id,
        author,
        Timestamp::from(created_at),
        kind,
        content,
        extra_tags,
        &previous,
    )
    .map_err(FfiError::from)?;

    Ok(Arc::new(FfiComposedWriteIntent {
        inner: Mutex::new(Some(intent)),
    }))
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
