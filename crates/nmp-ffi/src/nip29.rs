//! Native NIP-29 projection (#1015): one opaque [`NmpGroup`] identity that
//! mints ordinary demand and publishes through the canonical tracked engine
//! door.
//!
//! The selected host and group id cross only at construction. Publication
//! methods accept neither of them, no routing value, and no `h`/`previous`
//! field. An unsigned builder is contextualized by `nmp_nip29::Group`; a
//! pre-signed event is validated by that same owner without changing bytes or
//! event id. Every successful publication returns the ordinary
//! [`NmpReceiptStream`], with optional ordinary correlation and no parallel
//! receipt, retry, signer, store, or transport lifecycle.
//!
//! NIP-29's own named operations are methods on the same group object. Their
//! kind and tag schemas stay in `nmp-nip29`; this boundary accepts semantic
//! parameters only. Foreign schemas such as C7, NIP-25, and TTS29 remain
//! independently owned and reach the kind-blind [`Self::publish`] door as an
//! `FfiEventBuilder`.

use std::sync::Arc;

use nmp::GroupOperations;
use nostr::{EventId, RelayUrl};

use crate::convert::{
    demand_to_ffi, event_builder_from_ffi, filter_from_ffi, parse_correlation_token, parse_pubkey,
    signed_event_from_ffi, FfiError,
};
use crate::facade::{NmpEngine, NmpReceiptStream};
use crate::types::{FfiDemand, FfiEventBuilder, FfiFilter, FfiSignedEvent};

fn parse_host(host: String) -> Result<RelayUrl, FfiError> {
    RelayUrl::parse(&host).map_err(|_| FfiError::InvalidRelayUrl { got: host })
}

fn group_context_error(error: nmp::nip29::GroupContextError) -> FfiError {
    match error {
        nmp::nip29::GroupContextError::CallerSuppliedContext => {
            FfiError::GroupCallerSuppliedContext
        }
        nmp::nip29::GroupContextError::CallerSuppliedTimeline => {
            FfiError::GroupCallerSuppliedTimeline
        }
        nmp::nip29::GroupContextError::MissingContext { expected } => {
            FfiError::GroupMissingContext { expected }
        }
        nmp::nip29::GroupContextError::MismatchedContext { found, expected } => {
            FfiError::GroupMismatchedContext { found, expected }
        }
        nmp::nip29::GroupContextError::AmbiguousContext { expected } => {
            FfiError::GroupAmbiguousContext { expected }
        }
    }
}

fn group_publish_error(error: nmp::GroupPublishError) -> FfiError {
    match error {
        nmp::GroupPublishError::Context(error) => group_context_error(error),
        nmp::GroupPublishError::Engine(error) => error.into(),
    }
}

fn parse_optional_correlation(
    value: Option<String>,
) -> Result<Option<nmp::CorrelationToken>, FfiError> {
    value.as_deref().map(parse_correlation_token).transpose()
}

/// Group discovery (kind:39000) pinned to `host` (#108,
/// `nmp_nip29::group_discovery_demand` mirror). This browsing helper remains
/// independent of a concrete group id.
#[uniffi::export]
pub fn group_discovery_demand(host: String) -> Result<FfiDemand, FfiError> {
    Ok(demand_to_ffi(nmp_nip29::group_discovery_demand(
        parse_host(host)?,
    )))
}

/// One NIP-29 group identity: host plus group id, retained opaquely.
///
/// Construction performs no I/O, opens no observation, and requires no
/// account. The object exposes no host/id getter because publication-time
/// authority comes from this retained identity, never caller-supplied fields.
#[derive(uniffi::Object)]
pub struct NmpGroup {
    group: nmp::nip29::Group,
}

impl NmpGroup {
    fn publish_builder(
        &self,
        engine: &NmpEngine,
        builder: nmp::EventBuilder,
        correlation: Option<String>,
    ) -> Result<Arc<NmpReceiptStream>, FfiError> {
        let receipt = self
            .group
            .publish_tracked(
                &engine.engine,
                builder,
                parse_optional_correlation(correlation)?,
            )
            .map_err(group_publish_error)?;
        Ok(NmpReceiptStream::new(receipt))
    }
}

#[uniffi::export]
impl NmpGroup {
    /// Construct a group identity. No network/store/signer work is started.
    #[uniffi::constructor]
    pub fn new(host: String, group_id: String) -> Result<Arc<Self>, FfiError> {
        Ok(Arc::new(Self {
            group: nmp::nip29::Group::new(parse_host(host)?, group_id),
        }))
    }

    /// Add this group's host pin and `#h` scope to an app-selected filter.
    /// The result goes through the ordinary `NmpEngine::observe_demand` door.
    pub fn demand(&self, selection: FfiFilter) -> Result<FfiDemand, FfiError> {
        Ok(demand_to_ffi(
            self.group.demand(filter_from_ffi(selection)?),
        ))
    }

    /// Contextualize and publish an arbitrary unsigned builder through the
    /// ordinary tracked engine lifecycle.
    pub fn publish(
        &self,
        engine: Arc<NmpEngine>,
        builder: FfiEventBuilder,
        correlation: Option<String>,
    ) -> Result<Arc<NmpReceiptStream>, FfiError> {
        self.publish_builder(&engine, event_builder_from_ffi(builder)?, correlation)
    }

    /// Validate and publish a pre-signed event without changing its bytes,
    /// signature, tags, or event id.
    pub fn publish_signed(
        &self,
        engine: Arc<NmpEngine>,
        event: FfiSignedEvent,
        correlation: Option<String>,
    ) -> Result<Arc<NmpReceiptStream>, FfiError> {
        let event = signed_event_from_ffi(
            event.id,
            event.pubkey,
            event.created_at,
            event.kind,
            event.tags,
            event.content,
            event.sig,
        )?;
        let receipt = self
            .group
            .publish_signed_tracked(
                &engine.engine,
                event,
                parse_optional_correlation(correlation)?,
            )
            .map_err(group_publish_error)?;
        Ok(NmpReceiptStream::new(receipt))
    }

    /// kind:9021 -- request admission, optionally redeeming an invite code.
    pub fn join_request(
        &self,
        engine: Arc<NmpEngine>,
        invite_code: Option<String>,
        correlation: Option<String>,
    ) -> Result<Arc<NmpReceiptStream>, FfiError> {
        self.publish_builder(
            &engine,
            nmp::nip29::join_request(invite_code.as_deref()),
            correlation,
        )
    }

    /// kind:9022 -- leave this group.
    pub fn leave_request(
        &self,
        engine: Arc<NmpEngine>,
        correlation: Option<String>,
    ) -> Result<Arc<NmpReceiptStream>, FfiError> {
        self.publish_builder(&engine, nmp::nip29::leave_request(), correlation)
    }

    /// kind:9000 -- add a member, optionally with a role.
    pub fn add_user(
        &self,
        engine: Arc<NmpEngine>,
        pubkey: String,
        role: Option<String>,
        correlation: Option<String>,
    ) -> Result<Arc<NmpReceiptStream>, FfiError> {
        self.publish_builder(
            &engine,
            nmp::nip29::add_user(parse_pubkey(&pubkey)?, role.as_deref()),
            correlation,
        )
    }

    /// kind:9001 -- remove a member.
    pub fn remove_user(
        &self,
        engine: Arc<NmpEngine>,
        pubkey: String,
        correlation: Option<String>,
    ) -> Result<Arc<NmpReceiptStream>, FfiError> {
        self.publish_builder(
            &engine,
            nmp::nip29::remove_user(parse_pubkey(&pubkey)?),
            correlation,
        )
    }

    /// kind:9002 -- update supplied metadata fields; omitted fields remain
    /// absent from the operation.
    pub fn edit_metadata(
        &self,
        engine: Arc<NmpEngine>,
        name: Option<String>,
        about: Option<String>,
        correlation: Option<String>,
    ) -> Result<Arc<NmpReceiptStream>, FfiError> {
        self.publish_builder(
            &engine,
            nmp::nip29::edit_metadata(name.as_deref(), about.as_deref()),
            correlation,
        )
    }

    /// kind:9005 -- delete one group-hosted event.
    pub fn delete_event(
        &self,
        engine: Arc<NmpEngine>,
        event_id: String,
        correlation: Option<String>,
    ) -> Result<Arc<NmpReceiptStream>, FfiError> {
        let event_id =
            EventId::from_hex(&event_id).map_err(|_| FfiError::InvalidEventId { got: event_id })?;
        self.publish_builder(&engine, nmp::nip29::delete_event(event_id), correlation)
    }

    /// kind:9007 -- create this group at its retained host.
    pub fn create_group(
        &self,
        engine: Arc<NmpEngine>,
        correlation: Option<String>,
    ) -> Result<Arc<NmpReceiptStream>, FfiError> {
        self.publish_builder(&engine, nmp::nip29::create_group(), correlation)
    }

    /// kind:9008 -- delete this group from its retained host.
    pub fn delete_group(
        &self,
        engine: Arc<NmpEngine>,
        correlation: Option<String>,
    ) -> Result<Arc<NmpReceiptStream>, FfiError> {
        self.publish_builder(&engine, nmp::nip29::delete_group(), correlation)
    }

    /// kind:9009 -- create an invite code.
    pub fn create_invite(
        &self,
        engine: Arc<NmpEngine>,
        code: String,
        correlation: Option<String>,
    ) -> Result<Arc<NmpReceiptStream>, FfiError> {
        self.publish_builder(&engine, nmp::nip29::create_invite(&code), correlation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::NmpEngineConfig;
    use crate::types::{FfiBinding, FfiSourceAuthority, FfiWriteStatus};
    use nostr::{JsonUtil, Keys, Kind, Tag};
    use std::time::Duration;

    const TEST_SECRET_KEY_HEX: &str =
        "0000000000000000000000000000000000000000000000000000000000000001";

    fn group() -> Arc<NmpGroup> {
        NmpGroup::new(
            "wss://groups.example.com".to_string(),
            "photographers".to_string(),
        )
        .unwrap()
    }

    fn engine_with_account(config: NmpEngineConfig) -> Arc<NmpEngine> {
        let engine = NmpEngine::new(config).unwrap();
        let registration = engine.add_account(TEST_SECRET_KEY_HEX.to_string()).unwrap();
        engine
            .set_active_account(Some(registration.public_key()))
            .unwrap();
        engine
    }

    async fn next_status(stream: &NmpReceiptStream) -> FfiWriteStatus {
        tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("receipt status arrives within the test bound")
            .expect("receipt pull is not concurrent")
            .expect("receipt remains open until at least one fact")
    }

    fn signed(tags: Vec<Tag>) -> FfiSignedEvent {
        let keys = Keys::parse(TEST_SECRET_KEY_HEX).unwrap();
        let event = nostr::EventBuilder::new(Kind::from(9u16), "first light")
            .tags(tags)
            .custom_created_at(nostr::Timestamp::from(1_700_000_000u64))
            .sign_with_keys(&keys)
            .unwrap();
        FfiSignedEvent {
            id: event.id.to_hex(),
            pubkey: event.pubkey.to_hex(),
            created_at: event.created_at.as_secs(),
            kind: event.kind.as_u16(),
            tags: event
                .tags
                .iter()
                .map(|tag| tag.as_slice().to_vec())
                .collect(),
            content: event.content,
            sig: event.sig.to_string(),
        }
    }

    #[test]
    fn group_identity_constructs_without_an_engine_and_mints_ordinary_demand() {
        let group = group();
        let demand = group
            .demand(FfiFilter {
                kinds: Some(vec![9, 7]),
                ..FfiFilter::default()
            })
            .unwrap();
        assert_eq!(demand.selection.kinds, Some(vec![7, 9]));
        assert_eq!(
            demand.selection.tags.get("h"),
            Some(&FfiBinding::Literal {
                values: vec!["photographers".to_string()]
            })
        );
        assert_eq!(
            demand.source,
            FfiSourceAuthority::Pinned {
                relays: vec!["wss://groups.example.com".to_string()]
            }
        );
    }

    #[tokio::test]
    async fn unsigned_publication_refuses_caller_owned_group_rows_before_receipt() {
        let engine = engine_with_account(NmpEngineConfig::default());
        for (tag, expected) in [
            (
                vec!["h".to_string(), "photographers".to_string()],
                FfiError::GroupCallerSuppliedContext,
            ),
            (
                vec!["previous".to_string(), "deadbeef".to_string()],
                FfiError::GroupCallerSuppliedTimeline,
            ),
        ] {
            let result = group().publish(
                engine.clone(),
                FfiEventBuilder {
                    kind: 9,
                    tags: vec![tag],
                    content: "refuse me".to_string(),
                    created_at: None,
                },
                None,
            );
            assert!(matches!(result, Err(error) if error == expected));
        }
        engine.shutdown();
    }

    #[tokio::test]
    async fn tracked_group_publication_returns_an_ordinary_correlated_receipt() {
        let temp = tempfile::tempdir().unwrap();
        let store = temp.path().join("nmp.redb");
        let engine = engine_with_account(NmpEngineConfig {
            store_path: Some(store.to_string_lossy().into_owned()),
            ..NmpEngineConfig::default()
        });
        let receipt = group()
            .publish(
                engine.clone(),
                FfiEventBuilder {
                    kind: 9,
                    tags: Vec::new(),
                    content: "first light".to_string(),
                    created_at: Some(1_700_000_000),
                },
                Some("native-group-correlation".to_string()),
            )
            .unwrap();
        assert_eq!(next_status(&receipt).await, FfiWriteStatus::Accepted);
        let reattached = engine
            .reattach_by_correlation("native-group-correlation".to_string())
            .unwrap();
        assert_eq!(reattached.receipt_id, Some(receipt.id()));
        engine.shutdown();
    }

    #[tokio::test]
    async fn pre_signed_validation_is_typed_and_valid_bytes_keep_their_event_id() {
        let engine = engine_with_account(NmpEngineConfig::default());
        let group = group();

        let missing = signed(Vec::new());
        assert!(matches!(
            group.publish_signed(engine.clone(), missing, None),
            Err(FfiError::GroupMissingContext { expected })
                if expected == "photographers"
        ));

        let mismatched = signed(vec![Tag::parse(["h", "darkroom"]).unwrap()]);
        assert!(matches!(
            group.publish_signed(engine.clone(), mismatched, None),
            Err(FfiError::GroupMismatchedContext { found, expected })
                if found == "darkroom" && expected == "photographers"
        ));

        let ambiguous = signed(vec![
            Tag::parse(["h", "photographers"]).unwrap(),
            Tag::parse(["h", "darkroom"]).unwrap(),
        ]);
        assert!(matches!(
            group.publish_signed(engine.clone(), ambiguous, None),
            Err(FfiError::GroupAmbiguousContext { expected })
                if expected == "photographers"
        ));

        let valid = signed(vec![Tag::parse(["h", "photographers"]).unwrap()]);
        let expected_id = valid.id.clone();
        let expected = valid.clone();
        let receipt = group
            .publish_signed(engine.clone(), valid.clone(), None)
            .unwrap();
        let mut saw_same_id = false;
        for _ in 0..4 {
            match next_status(&receipt).await {
                FfiWriteStatus::Signed { event_id } => {
                    assert_eq!(event_id, expected_id);
                    saw_same_id = true;
                    break;
                }
                FfiWriteStatus::Accepted | FfiWriteStatus::Routed { .. } => {}
                other => panic!("expected acceptance/signing evidence, got {other:?}"),
            }
        }
        assert!(saw_same_id, "the existing signed event id must survive");
        assert_eq!(valid, expected, "the caller's FFI value is not mutated");
        engine.shutdown();
    }

    #[tokio::test]
    async fn every_typed_operation_enters_the_same_tracked_receipt_lifecycle() {
        let engine = engine_with_account(NmpEngineConfig::default());
        let group = group();
        let pubkey = Keys::generate().public_key().to_hex();
        let event_id = "09".repeat(32);
        let receipts = vec![
            group.join_request(engine.clone(), Some("invite".to_string()), None),
            group.leave_request(engine.clone(), None),
            group.add_user(
                engine.clone(),
                pubkey.clone(),
                Some("admin".to_string()),
                None,
            ),
            group.remove_user(engine.clone(), pubkey, None),
            group.edit_metadata(
                engine.clone(),
                Some("Photographers".to_string()),
                Some("film only".to_string()),
                None,
            ),
            group.delete_event(engine.clone(), event_id, None),
            group.create_group(engine.clone(), None),
            group.delete_group(engine.clone(), None),
            group.create_invite(engine.clone(), "invite".to_string(), None),
        ];
        for receipt in receipts {
            let receipt = receipt.expect("semantic operation reaches the tracked publish door");
            assert_eq!(next_status(&receipt).await, FfiWriteStatus::Accepted);
        }
        engine.shutdown();
    }

    #[test]
    fn invalid_constructor_and_semantic_inputs_keep_the_shared_typed_errors() {
        assert!(matches!(
            NmpGroup::new("not-a-relay".to_string(), "photographers".to_string()),
            Err(FfiError::InvalidRelayUrl { got }) if got == "not-a-relay"
        ));
        let engine = engine_with_account(NmpEngineConfig::default());
        assert!(matches!(
            group().add_user(engine.clone(), "not-a-key".to_string(), None, None),
            Err(FfiError::InvalidPublicKey { got }) if got == "not-a-key"
        ));
        assert!(matches!(
            group().publish(
                engine.clone(),
                FfiEventBuilder {
                    kind: 9,
                    tags: Vec::new(),
                    content: String::new(),
                    created_at: None,
                },
                Some(String::new()),
            ),
            Err(FfiError::InvalidCorrelationToken { got, .. }) if got.is_empty()
        ));
        engine.shutdown();
    }

    #[test]
    fn signed_fixture_is_canonical_nostr_json() {
        let event = signed(vec![Tag::parse(["h", "photographers"]).unwrap()]);
        let json = serde_json::json!({
            "id": event.id,
            "pubkey": event.pubkey,
            "created_at": event.created_at,
            "kind": event.kind,
            "tags": event.tags,
            "content": event.content,
            "sig": event.sig,
        });
        let parsed = nostr::Event::from_json(json.to_string()).unwrap();
        parsed.verify().unwrap();
    }
}
