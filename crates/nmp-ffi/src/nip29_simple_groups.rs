//! NIP-51 Simple groups, exposed with the NIP-29 product capability:
//! tolerant, observational parsing at the FFI boundary (#863/#1551).
//!
//! An [`FfiRow`] crossing this boundary is CALLER-CONSTRUCTIBLE -- a native
//! app can invent every field, including `kind`, `sig`, and `sources`. So
//! the only honest thing this module can offer is a tolerant reader whose
//! result is plain data: `parse_simple_groups_list_tolerant` names that in
//! the API itself, and [`FfiSimpleGroupsList`] documents it in the type.
//!
//! Deliberately absent: any observation-qualified `Observed*` wrapper,
//! projection-error family, frame-proof projector, or other
//! protocol-specific witness. Group-list reading stays the ordinary
//! `LiveQuery`/`FfiDemand` noun ([`current_account_group_list_demand`]). The
//! typed add/remove methods below compile private operation bytes through the
//! Rust-owned durable semantic-write machinery and return the ordinary
//! [`NmpReceiptStream`]; no observed-authority or action-lifecycle noun crosses
//! this boundary.
//!
//! Also deliberately absent since #858: any second projection of this value.
//! [`FfiSimpleGroupsList`] is the ONE native shape a decoded kind:10009 list
//! takes. The NIP-29-facing wrapper family that used to sit beside it merely
//! renamed these fields and dropped `malformed_item_count` -- exactly the
//! second-owner shape #63's boundary exists to forbid. A caller that wants to
//! browse a group picks one [`FfiSimpleGroupEntry`] and passes its
//! `host_relay`/`group_id` to `crate::nip29`'s constructors itself.

use std::sync::Arc;

use nostr::RelayUrl;

use crate::convert::demand_to_ffi;
use crate::facade::{NmpEngine, NmpReceiptStream};
use crate::types::{FfiDemand, FfiRow, FfiSimpleGroupEntry, FfiSimpleGroupsList};

/// A typed group-list action was refused before ordinary receipt custody.
/// `EngineClosed` and `PublishRefused` name exactly what
/// [`nmp_nip29::GroupListActionError`] itself can carry -- there is no
/// separate group-list-only fiction standing in for a receipt that failed to
/// materialize for no named reason.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum FfiGroupListActionError {
    InvalidRelayUrl { got: String },
    AutomaticRoutingUnavailable,
    SignedOut,
    EngineClosed,
    PublishRefused { reason: String },
}

impl std::fmt::Display for FfiGroupListActionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRelayUrl { got } => write!(f, "invalid relay URL: {got}"),
            Self::AutomaticRoutingUnavailable => {
                f.write_str("automatic author/outbox routing is not configured")
            }
            Self::SignedOut => f.write_str("no current account is selected"),
            Self::EngineClosed => f.write_str("the engine is closed"),
            Self::PublishRefused { reason } => write!(f, "{reason}"),
        }
    }
}

impl std::error::Error for FfiGroupListActionError {}

impl From<nmp_nip29::GroupListActionError> for FfiGroupListActionError {
    fn from(error: nmp_nip29::GroupListActionError) -> Self {
        match error {
            nmp_nip29::GroupListActionError::SignedOut => Self::SignedOut,
            nmp_nip29::GroupListActionError::EngineClosed => Self::EngineClosed,
            nmp_nip29::GroupListActionError::PublishRefused { reason } => {
                Self::PublishRefused { reason }
            }
        }
    }
}

fn parse_action_relay(relay: String) -> Result<RelayUrl, FfiGroupListActionError> {
    RelayUrl::parse(&relay).map_err(|_| FfiGroupListActionError::InvalidRelayUrl { got: relay })
}

fn require_group_list_routing(engine: &NmpEngine) -> Result<(), FfiGroupListActionError> {
    if engine.automatic_routing == crate::facade::AutomaticRoutingAssembly::Unavailable {
        return Err(FfiGroupListActionError::AutomaticRoutingUnavailable);
    }
    Ok(())
}

fn simple_group_entry_to_ffi(entry: &nmp_nip29::SimpleGroupEntry) -> FfiSimpleGroupEntry {
    FfiSimpleGroupEntry {
        group_id: entry.group_id.clone(),
        host_relay: entry.host_relay.to_string(),
        name: entry.name.clone(),
    }
}

fn simple_groups_list_to_ffi(list: &nmp_nip29::SimpleGroupsList) -> FfiSimpleGroupsList {
    FfiSimpleGroupsList {
        items: list.items.iter().map(simple_group_entry_to_ffi).collect(),
        relays_in_use: list.relays_in_use.iter().map(RelayUrl::to_string).collect(),
        malformed_item_count: u64::try_from(list.malformed_item_count)
            .expect("usize always fits u64 on supported FFI targets"),
        has_private_content: list.has_private_content,
    }
}

/// The signed-in account's Simple-groups-list demand (#108,
/// `nmp_nip29::current_account_group_list_demand` mirror): `kinds:[10009]`,
/// `AuthorOutboxes + Public`. Signed-out (no current account) resolves to
/// zero atoms through the ordinary reactive-binding empty-resolution path
/// -- no special case needed on either side of this boundary.
///
/// #1551 places this NIP-51-defined list with the NIP-29 product capability
/// that consumes it, without changing which NIP defines kind:10009.
#[uniffi::export]
pub fn current_account_group_list_demand() -> FfiDemand {
    demand_to_ffi(nmp_nip29::current_account_group_list_demand())
}

/// Tolerantly parse Simple-groups-shaped public items out of a raw native
/// row (#863). Infallible, and deliberately kind-agnostic: `row` may carry
/// any `kind`, an invented `sig`, and no `sources` at all.
///
/// The result preserves malformed-item and private-content evidence, and
/// grants NO signature, canonical-store, provenance, routing, or mutation
/// authority. To discover NIP-29 groups the app still passes an explicit host
/// set of its own choosing to `FfiRelayScope::on`; nothing here authorizes a
/// host or invents a fixed group-content catalog on the app's behalf.
#[uniffi::export]
pub fn parse_simple_groups_list_tolerant(row: FfiRow) -> FfiSimpleGroupsList {
    simple_groups_list_to_ffi(&nmp_nip29::parse_simple_groups_list_from_raw_tags_tolerant(
        row.tags.iter().map(|tag| tag.as_slice()),
        &row.content,
    ))
}

#[uniffi::export]
impl NmpEngine {
    /// Add one public group-list identity through the ordinary durable write
    /// and receipt lifecycle. The host inside the event never becomes a
    /// publication route; this kind:10009 uses the selected author's outbox.
    pub fn add_group_to_list(
        &self,
        group_id: String,
        host_relay: String,
        name: Option<String>,
    ) -> Result<Arc<NmpReceiptStream>, FfiGroupListActionError> {
        require_group_list_routing(self)?;
        let group = nmp_nip29::SimpleGroupEntry {
            group_id,
            host_relay: parse_action_relay(host_relay)?,
            name,
        };
        let receipt = nmp_nip29::add_group_to_list(&self.engine, &self.group_list_writes, group)?;
        Ok(NmpReceiptStream::new(self.engine.clone(), receipt))
    }

    /// Remove every valid public group tag with this exact `(id, host)`
    /// identity while preserving malformed, unrelated, and private data.
    pub fn remove_group_from_list(
        &self,
        group_id: String,
        host_relay: String,
    ) -> Result<Arc<NmpReceiptStream>, FfiGroupListActionError> {
        require_group_list_routing(self)?;
        let receipt = nmp_nip29::remove_group_from_list(
            &self.engine,
            &self.group_list_writes,
            group_id,
            parse_action_relay(host_relay)?,
        )?;
        Ok(NmpReceiptStream::new(self.engine.clone(), receipt))
    }

    /// Add one canonical public relay-in-use tag when it is not already
    /// present. Group tags are outside this operation's ownership.
    pub fn add_relay_in_use(
        &self,
        relay: String,
    ) -> Result<Arc<NmpReceiptStream>, FfiGroupListActionError> {
        require_group_list_routing(self)?;
        let receipt = nmp_nip29::add_relay_in_use(
            &self.engine,
            &self.group_list_writes,
            parse_action_relay(relay)?,
        )?;
        Ok(NmpReceiptStream::new(self.engine.clone(), receipt))
    }

    /// Remove every valid equivalent public relay-in-use tag. Group tags and
    /// malformed relay tags remain untouched.
    pub fn remove_relay_in_use(
        &self,
        relay: String,
    ) -> Result<Arc<NmpReceiptStream>, FfiGroupListActionError> {
        require_group_list_routing(self)?;
        let receipt = nmp_nip29::remove_relay_in_use(
            &self.engine,
            &self.group_list_writes,
            parse_action_relay(relay)?,
        )?;
        Ok(NmpReceiptStream::new(self.engine.clone(), receipt))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::{FfiOutboxRoutingConfig, NmpEngineConfig};
    use crate::session::FfiPrivateKey;

    fn routed_engine() -> Arc<NmpEngine> {
        NmpEngine::new(
            NmpEngineConfig {
                outbox_routing: Some(FfiOutboxRoutingConfig {
                    indexers: vec!["wss://indexer.example".to_string()],
                }),
                ..NmpEngineConfig::default()
            },
            None,
        )
        .expect("a nonempty app-owned indexer set constructs")
    }

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
            signature: crate::types::FfiRowSignature::Signed {
                signature: "caller-chosen-signature".to_owned(),
            },
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
    fn current_account_group_list_demand_projects_the_reactive_authors_binding() {
        let demand = current_account_group_list_demand();
        assert_eq!(demand.selection.kinds, Some(vec![10009]));
    }

    #[test]
    fn group_list_actions_refuse_invalid_relays_and_missing_routing_before_custody() {
        let routed = routed_engine();
        let malformed =
            match routed.add_group_to_list("room".to_string(), "not-a-relay".to_string(), None) {
                Err(error) => error,
                Ok(_) => panic!("a malformed relay must refuse before returning a receipt"),
            };
        assert_eq!(
            malformed,
            FfiGroupListActionError::InvalidRelayUrl {
                got: "not-a-relay".to_string()
            }
        );
        assert!(routed.publish_queue(None, u8::MAX).unwrap().is_empty());
        routed.shutdown();

        let providerless = NmpEngine::new(NmpEngineConfig::default(), None)
            .expect("an explicit-routing-only engine is valid");
        providerless
            .add_private_key_account(FfiPrivateKey::generate(), true)
            .expect("the native account registers");
        let providerless_error = match providerless
            .add_relay_in_use("wss://relay.example".to_string())
        {
            Err(error) => error,
            Ok(_) => panic!("providerless automatic routing must refuse before receipt custody"),
        };
        assert_eq!(
            providerless_error,
            FfiGroupListActionError::AutomaticRoutingUnavailable
        );
        assert!(providerless
            .publish_queue(None, u8::MAX)
            .unwrap()
            .is_empty());
        providerless.shutdown();
    }

    #[test]
    fn group_list_actions_refuse_signed_out_and_return_the_ordinary_receipt() {
        let engine = routed_engine();
        let signed_out = match engine.add_group_to_list(
            "room".to_string(),
            "wss://host.example".to_string(),
            Some("Room".to_string()),
        ) {
            Err(error) => error,
            Ok(_) => panic!("signed-out group-list action must refuse before receipt custody"),
        };
        assert_eq!(signed_out, FfiGroupListActionError::SignedOut);
        assert!(engine.publish_queue(None, u8::MAX).unwrap().is_empty());

        engine
            .add_private_key_account(FfiPrivateKey::generate(), true)
            .expect("the native account registers");
        let receipt = engine
            .add_group_to_list(
                "room".to_string(),
                "wss://host.example".to_string(),
                Some("Room".to_string()),
            )
            .expect("the first list value enters ordinary custody");
        let relay_receipt = engine
            .add_relay_in_use("wss://relay.example".to_string())
            .expect("relay-in-use addition enters ordinary custody");
        let remove_group_receipt = engine
            .remove_group_from_list("room".to_string(), "wss://host.example".to_string())
            .expect("group removal enters ordinary custody");
        let remove_relay_receipt = engine
            .remove_relay_in_use("wss://relay.example".to_string())
            .expect("relay-in-use removal enters ordinary custody");
        let queue = engine.publish_queue(None, u8::MAX).unwrap();
        assert_eq!(queue.len(), 4);
        assert_eq!(
            queue
                .iter()
                .map(|entry| entry.receipt_id)
                .collect::<Vec<_>>(),
            vec![
                receipt.id(),
                relay_receipt.id(),
                remove_group_receipt.id(),
                remove_relay_receipt.id(),
            ]
        );
        engine.shutdown();
    }

    /// The host an app browses with is its own explicit typed input, never
    /// harvested from parser output by the boundary itself.
    ///
    /// #1033's FFI falsifier too (successor to #858's, updated for the
    /// `FfiRelayScope`/`FfiGroup` projection):
    /// the SELECTED entry's `host_relay` AND `group_id` both feed NIP-29's
    /// host-pinned constructors directly, field for field, with no
    /// intermediate NIP-29 group-reference copy of the NIP-51 value in between.
    #[test]
    #[cfg(feature = "nip29")]
    fn nip29_browsing_still_demands_an_explicitly_supplied_host() {
        use crate::types::{FfiFilter, FfiSourceAuthority};

        let list = parse_simple_groups_list_tolerant(fabricated_row(10_009));
        let selected = list.items[0].clone();
        let scope = crate::nip29::FfiRelayScope::on(vec![selected.host_relay.clone()])
            .expect("app-supplied host parses");
        let group = scope.group(selected.group_id.clone());
        let query = group
            .read(FfiFilter::default())
            .expect("a single-host group read is one branch");
        assert_eq!(query.branches.len(), 1);
        assert_eq!(
            query.branches[0].source,
            FfiSourceAuthority::Pinned {
                relays: vec![selected.host_relay.clone()]
            }
        );

        assert_eq!(
            selected.group_id, "group-a",
            "the NIP-29-owned group id remains caller data; NIP-29 does not \
             turn it into a fixed content catalog"
        );
    }
}
