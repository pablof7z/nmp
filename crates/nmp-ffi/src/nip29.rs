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
//! `NmpEngine::group_message_intent`/[`FfiComposedWriteIntent`] (#156) are
//! this module's write-side counterpart: an app supplies semantic composer
//! state while NMP owns author/time/kind, mention materialization,
//! `p`/reply-`e`, `h`/`previous`, and pinned-host routing. The app receives
//! only the opaque, take-once handle `NmpEngine::publish_composed` consumes.

use std::sync::{Arc, Mutex};

use nostr::{EventId, RelayUrl};

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

/// Typed reply contribution for an ordinary kind:9 group message (#156).
/// Native callers never spell the corresponding `e`/`p` rows.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct FfiGroupReplyParent {
    pub event_id: String,
    pub author_pubkey: String,
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
    pub(crate) fn new(intent: nmp_grammar::WriteIntent) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Some(intent)),
        })
    }

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

/// Parse native semantic inputs and delegate to NMP's typed kind:9 group-
/// message operation (#156). This is intentionally crate-private: the UniFFI
/// surface is [`crate::facade::NmpEngine::group_message_intent`], because the
/// active author and wall-clock are engine/NMP state rather than caller
/// parameters.
pub(crate) fn group_message_intent(
    engine: &nmp::Engine,
    host: String,
    group_id: String,
    content: String,
    recipient_pubkeys: Vec<String>,
    reply_to: Option<FfiGroupReplyParent>,
    recent_rows: Vec<FfiRow>,
) -> Result<Arc<FfiComposedWriteIntent>, FfiError> {
    let host = parse_host(host)?;
    let recipients = recipient_pubkeys
        .iter()
        .map(|pubkey| parse_pubkey(pubkey))
        .collect::<Result<Vec<_>, _>>()?;
    let reply_to = reply_to
        .map(|parent| {
            let event_id =
                EventId::from_hex(&parent.event_id).map_err(|_| FfiError::InvalidEventId {
                    got: parent.event_id.clone(),
                })?;
            let author = parse_pubkey(&parent.author_pubkey)?;
            Ok::<_, FfiError>(nmp_nip29::GroupReplyParent { event_id, author })
        })
        .transpose()?;

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

    let intent = nmp_nip29::compose_group_message(
        engine, host, &group_id, content, recipients, reply_to, &previous,
    )
    .map_err(FfiError::from)?;

    Ok(FfiComposedWriteIntent::new(intent))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FfiSourceAuthority;
    use nmp::{EngineConfig, WritePayload};

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
    fn semantic_group_message_projects_mentions_and_reply_without_raw_native_inputs() {
        let engine = nmp::Engine::new(EngineConfig::default()).unwrap();
        let author = nostr::Keys::generate().public_key();
        engine.set_active_account(Some(author)).unwrap();
        let first = "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d";
        let second = "7e7e9c42a91bfef19fa929e5fda1b72e0ebc1a4c1141673e2794234d86addf4e";
        let parent_id = "11".repeat(32);

        let wrapped = group_message_intent(
            &engine,
            "wss://group-host.example.com".to_string(),
            "group-a".to_string(),
            "hello".to_string(),
            vec![first.to_string(), first.to_string(), second.to_string()],
            Some(FfiGroupReplyParent {
                event_id: parent_id.clone(),
                author_pubkey: first.to_string(),
            }),
            vec![],
        )
        .unwrap();
        let intent = wrapped.take().unwrap();
        let WritePayload::Unsigned(unsigned) = intent.payload else {
            panic!("semantic group messages must produce unsigned intents")
        };

        assert_eq!(unsigned.pubkey, author);
        assert_eq!(unsigned.kind, nostr::Kind::from(9u16));
        assert_eq!(
            unsigned.content,
            concat!(
                "nostr:npub180cvv07tjdrrgpa0j7j7tmnyl2yr6yr7l8j4s3evf6u64th6gkwsyjh6w6 ",
                "nostr:npub10elfcs4fr0l0r8af98jlmgdh9c8tcxjvz9qkw038js35mp4dma8qzvjptg ",
                "hello"
            )
        );
        let rows = unsigned
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            vec![
                vec!["p".to_string(), first.to_string()],
                vec!["p".to_string(), second.to_string()],
                vec![
                    "e".to_string(),
                    parent_id,
                    String::new(),
                    "reply".to_string(),
                    first.to_string(),
                ],
                vec!["h".to_string(), "group-a".to_string()],
            ]
        );
        engine.shutdown();
    }

    #[test]
    fn semantic_group_message_requires_an_active_account() {
        let engine = nmp::Engine::new(EngineConfig::default()).unwrap();
        let result = group_message_intent(
            &engine,
            "wss://group-host.example.com".to_string(),
            "group-a".to_string(),
            "hello".to_string(),
            vec![],
            None,
            vec![],
        );
        match result {
            Err(error) => assert_eq!(error, FfiError::NoActiveAccount),
            Ok(_) => panic!("signed-out group composition must fail"),
        }
        engine.shutdown();
    }

    #[test]
    fn semantic_group_message_rejects_malformed_typed_inputs() {
        let engine = nmp::Engine::new(EngineConfig::default()).unwrap();
        engine
            .set_active_account(Some(nostr::Keys::generate().public_key()))
            .unwrap();
        let result = group_message_intent(
            &engine,
            "wss://group-host.example.com".to_string(),
            "group-a".to_string(),
            "hello".to_string(),
            vec!["not-a-pubkey".to_string()],
            None,
            vec![],
        );
        match result {
            Err(FfiError::InvalidPublicKey { got }) => assert_eq!(got, "not-a-pubkey"),
            Err(other) => panic!("expected InvalidPublicKey, got {other:?}"),
            Ok(_) => panic!("malformed recipients must fail"),
        }
        engine.shutdown();
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
