//! Durable semantic operations over the current account's kind:10009 list.
//!
//! `nmp-nip29` owns the pure NIP-29/NIP-51 tag vocabulary. This module lives
//! one layer up because a durable operation must also mint the ordinary
//! [`WriteIntent`](crate::WriteIntent), freeze the selected account, and enter
//! the engine's receipt lifecycle. The dependency remains `nmp -> nmp-nip29`.

use nmp_grammar::{EventBuilder, Identity, WriteIntent, WriteRouting};
use nostr::{Kind, RelayUrl, Tag, Timestamp, UnsignedEvent};
use serde::{Deserialize, Serialize};

use crate::{
    Engine, EngineError, ReceiptStream, RegisteredReplaceableMaterializer, ReplaceableMaterializer,
    ReplaceableMaterializerOperation, ReplaceableMaterializerRefusal, ReplaceableMaterializerSpec,
};

use super::SimpleGroupEntry;

const GROUP_LIST_KIND: Kind = Kind::Custom(10009);
const GROUP_LIST_PROGRAM: [u8; 16] = *b"nmp-nip29-list!!";
const GROUP_LIST_FORMAT: [u8; 16] = *b"nip29-list-v001!";
const GROUP_LIST_OPERATION_VERSION: u8 = 1;

/// Compiled constructor for NIP-29 group-list operations.
///
/// The handle is opaque: apps name a typed operation below and receive the
/// ordinary write receipt. They never construct operation bytes, source
/// identities, contributor state, or materialization callbacks. Publishing
/// through an engine that was not constructed with [`group_list_capability`]
/// is refused before custody.
#[derive(Clone, Copy)]
pub struct GroupListWrites {
    registration: RegisteredReplaceableMaterializer,
}

/// Why a typed group-list action was refused before ordinary write custody.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupListActionError {
    SignedOut,
    EngineClosed,
    ReceiptUnavailable,
}

impl std::fmt::Display for GroupListActionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SignedOut => f.write_str("no current account is selected"),
            Self::EngineClosed => f.write_str("the engine is closed"),
            Self::ReceiptUnavailable => {
                f.write_str("the group-list operation was refused before receipt custody")
            }
        }
    }
}

impl std::error::Error for GroupListActionError {}

/// The compiled NIP-29 kind:10009 capability that must be supplied before
/// engine recovery.
#[must_use]
pub fn group_list_capability() -> ReplaceableMaterializerSpec {
    ReplaceableMaterializerSpec::new(GROUP_LIST_PROGRAM, GROUP_LIST_FORMAT, GroupListMaterializer)
}

/// Typed NIP-29 group-list constructor bound to [`group_list_capability`].
#[must_use]
pub fn group_list_writes() -> GroupListWrites {
    GroupListWrites {
        registration: group_list_capability().handle(),
    }
}

/// Append `group` when its exact `(group id, canonical host relay)` identity
/// is not already present. An existing optional display name is never
/// rewritten by an add.
pub fn add_group_to_list(
    engine: &Engine,
    writes: &GroupListWrites,
    group: SimpleGroupEntry,
) -> Result<ReceiptStream, GroupListActionError> {
    publish_operation(
        engine,
        writes,
        GroupListOperation::AddGroup {
            group_id: group.group_id,
            host_relay: group.host_relay,
            name: group.name,
        },
    )
}

/// Remove every valid public `group` tag with the exact identity. Malformed
/// near-matches and same-id groups hosted elsewhere remain byte-for-byte.
pub fn remove_group_from_list(
    engine: &Engine,
    writes: &GroupListWrites,
    group_id: String,
    host_relay: RelayUrl,
) -> Result<ReceiptStream, GroupListActionError> {
    publish_operation(
        engine,
        writes,
        GroupListOperation::RemoveGroup {
            group_id,
            host_relay,
        },
    )
}

/// Append one canonical public `r` tag unless an equivalent valid tag exists.
pub fn add_relay_in_use(
    engine: &Engine,
    writes: &GroupListWrites,
    relay: RelayUrl,
) -> Result<ReceiptStream, GroupListActionError> {
    publish_operation(engine, writes, GroupListOperation::AddRelay { relay })
}

/// Remove every valid public `r` tag equivalent to `relay`. Malformed tags
/// remain untouched.
pub fn remove_relay_in_use(
    engine: &Engine,
    writes: &GroupListWrites,
    relay: RelayUrl,
) -> Result<ReceiptStream, GroupListActionError> {
    publish_operation(engine, writes, GroupListOperation::RemoveRelay { relay })
}

fn publish_operation(
    engine: &Engine,
    writes: &GroupListWrites,
    operation: GroupListOperation,
) -> Result<ReceiptStream, GroupListActionError> {
    let author = match engine.session() {
        Ok(session) => session
            .current_pubkey
            .ok_or(GroupListActionError::SignedOut)?,
        Err(_) => return Err(GroupListActionError::EngineClosed),
    };
    let operation =
        encode_operation(&operation).map_err(|_| GroupListActionError::ReceiptUnavailable)?;
    let payload = writes
        .registration
        .first_value_operation(GROUP_LIST_KIND, String::new(), operation)
        .map_err(|_| GroupListActionError::ReceiptUnavailable)?;
    engine
        .publish(WriteIntent {
            payload,
            routing: WriteRouting::Auto,
            identity: Identity::Explicit(author),
            correlation: None,
        })
        .map_err(|error| match error {
            EngineError::EngineClosed => GroupListActionError::EngineClosed,
            _ => GroupListActionError::ReceiptUnavailable,
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum GroupListOperation {
    AddGroup {
        group_id: String,
        host_relay: RelayUrl,
        name: Option<String>,
    },
    RemoveGroup {
        group_id: String,
        host_relay: RelayUrl,
    },
    AddRelay {
        relay: RelayUrl,
    },
    RemoveRelay {
        relay: RelayUrl,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireOperation {
    version: u8,
    action: WireAction,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum WireAction {
    AddGroup {
        group_id: String,
        host_relay: String,
        name: Option<String>,
    },
    RemoveGroup {
        group_id: String,
        host_relay: String,
    },
    AddRelay {
        relay: String,
    },
    RemoveRelay {
        relay: String,
    },
}

fn encode_operation(operation: &GroupListOperation) -> Result<Vec<u8>, serde_json::Error> {
    let action = match operation {
        GroupListOperation::AddGroup {
            group_id,
            host_relay,
            name,
        } => WireAction::AddGroup {
            group_id: group_id.clone(),
            host_relay: host_relay.to_string(),
            name: name.clone(),
        },
        GroupListOperation::RemoveGroup {
            group_id,
            host_relay,
        } => WireAction::RemoveGroup {
            group_id: group_id.clone(),
            host_relay: host_relay.to_string(),
        },
        GroupListOperation::AddRelay { relay } => WireAction::AddRelay {
            relay: relay.to_string(),
        },
        GroupListOperation::RemoveRelay { relay } => WireAction::RemoveRelay {
            relay: relay.to_string(),
        },
    };
    serde_json::to_vec(&WireOperation {
        version: GROUP_LIST_OPERATION_VERSION,
        action,
    })
}

fn decode_operation(bytes: &[u8]) -> Result<GroupListOperation, String> {
    let wire: WireOperation = serde_json::from_slice(bytes)
        .map_err(|error| format!("NIP-29 group-list operation is malformed: {error}"))?;
    if wire.version != GROUP_LIST_OPERATION_VERSION {
        return Err("NIP-29 group-list operation has an unsupported version".to_string());
    }
    match wire.action {
        WireAction::AddGroup {
            group_id,
            host_relay,
            name,
        } => Ok(GroupListOperation::AddGroup {
            group_id,
            host_relay: parse_operation_relay(host_relay)?,
            name,
        }),
        WireAction::RemoveGroup {
            group_id,
            host_relay,
        } => Ok(GroupListOperation::RemoveGroup {
            group_id,
            host_relay: parse_operation_relay(host_relay)?,
        }),
        WireAction::AddRelay { relay } => Ok(GroupListOperation::AddRelay {
            relay: parse_operation_relay(relay)?,
        }),
        WireAction::RemoveRelay { relay } => Ok(GroupListOperation::RemoveRelay {
            relay: parse_operation_relay(relay)?,
        }),
    }
}

fn parse_operation_relay(relay: String) -> Result<RelayUrl, String> {
    RelayUrl::parse(&relay)
        .map_err(|_| "NIP-29 group-list operation has an invalid relay URL".to_string())
}

struct GroupListMaterializer;

impl ReplaceableMaterializer for GroupListMaterializer {
    fn materialize(
        &self,
        source: &UnsignedEvent,
        current: &UnsignedEvent,
        operations: &[ReplaceableMaterializerOperation<'_>],
    ) -> Result<EventBuilder, ReplaceableMaterializerRefusal> {
        if source.kind != GROUP_LIST_KIND
            || current.kind != GROUP_LIST_KIND
            || source.pubkey != current.pubkey
            || source.tags.identifier() != current.tags.identifier()
        {
            return Err(ReplaceableMaterializerRefusal {
                reason: "NIP-29 group-list materialization source coordinate changed".to_string(),
            });
        }
        apply_operations(current, operations)
    }

    fn materialize_default(
        &self,
        coordinate: &nostr::nips::nip01::Coordinate,
        operations: &[ReplaceableMaterializerOperation<'_>],
    ) -> Result<EventBuilder, ReplaceableMaterializerRefusal> {
        if coordinate.kind != GROUP_LIST_KIND || !coordinate.identifier.is_empty() {
            return Err(ReplaceableMaterializerRefusal {
                reason: "NIP-29 first-value coordinate is not a group list".to_string(),
            });
        }
        let empty = UnsignedEvent::new(
            coordinate.public_key,
            Timestamp::from(0),
            GROUP_LIST_KIND,
            Vec::new(),
            String::new(),
        );
        apply_operations(&empty, operations)
    }
}

fn apply_operations(
    current: &UnsignedEvent,
    operations: &[ReplaceableMaterializerOperation<'_>],
) -> Result<EventBuilder, ReplaceableMaterializerRefusal> {
    let mut tags = current.tags.clone().to_vec();
    for encoded in operations {
        let operation = decode_operation(encoded.bytes())
            .map_err(|reason| ReplaceableMaterializerRefusal { reason })?;
        match operation {
            GroupListOperation::AddGroup {
                group_id,
                host_relay,
                name,
            } => {
                if !tags
                    .iter()
                    .any(|tag| group_tag_matches(tag, &group_id, &host_relay))
                {
                    let mut row = vec!["group".to_string(), group_id, host_relay.to_string()];
                    if let Some(name) = name {
                        row.push(name);
                    }
                    tags.push(Tag::parse(row).map_err(|_| ReplaceableMaterializerRefusal {
                        reason: "NIP-29 group-list operation produced an invalid tag".to_string(),
                    })?);
                }
            }
            GroupListOperation::RemoveGroup {
                group_id,
                host_relay,
            } => tags.retain(|tag| !group_tag_matches(tag, &group_id, &host_relay)),
            GroupListOperation::AddRelay { relay } => {
                if !tags.iter().any(|tag| relay_tag_matches(tag, &relay)) {
                    tags.push(Tag::parse(["r", relay.as_str()]).map_err(|_| {
                        ReplaceableMaterializerRefusal {
                            reason: "NIP-29 group-list operation produced an invalid relay tag"
                                .to_string(),
                        }
                    })?);
                }
            }
            GroupListOperation::RemoveRelay { relay } => {
                tags.retain(|tag| !relay_tag_matches(tag, &relay));
            }
        }
    }
    Ok(EventBuilder {
        kind: GROUP_LIST_KIND,
        tags,
        content: current.content.clone(),
        created_at: None,
    })
}

fn group_tag_matches(tag: &Tag, group_id: &str, host_relay: &RelayUrl) -> bool {
    let row = tag.as_slice();
    row.first().is_some_and(|cell| cell == "group")
        && row.get(1).is_some_and(|cell| cell == group_id)
        && row
            .get(2)
            .and_then(|relay| RelayUrl::parse(relay).ok())
            .is_some_and(|relay| relay == *host_relay)
}

fn relay_tag_matches(tag: &Tag, expected: &RelayUrl) -> bool {
    let row = tag.as_slice();
    row.first().is_some_and(|cell| cell == "r")
        && row
            .get(1)
            .and_then(|relay| RelayUrl::parse(relay).ok())
            .is_some_and(|relay| relay == *expected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::Keys;

    fn operation(value: GroupListOperation) -> Vec<u8> {
        encode_operation(&value).unwrap()
    }

    fn operation_ref(bytes: &[u8]) -> ReplaceableMaterializerOperation<'_> {
        ReplaceableMaterializerOperation::new(bytes)
    }

    fn current(tags: Vec<Vec<&str>>, content: &str) -> UnsignedEvent {
        UnsignedEvent::new(
            Keys::generate().public_key(),
            Timestamp::from(10),
            GROUP_LIST_KIND,
            tags.into_iter()
                .map(|row| Tag::parse(row).unwrap())
                .collect::<Vec<_>>(),
            content,
        )
    }

    #[test]
    fn group_and_relay_operations_touch_only_their_exact_valid_tags() {
        let a = RelayUrl::parse("wss://a.example").unwrap();
        let b = RelayUrl::parse("wss://b.example").unwrap();
        let relay = RelayUrl::parse("wss://used.example").unwrap();
        let base = current(
            vec![
                vec!["x", "opaque", "cells"],
                vec!["group", "room", "wss://a.example", "Old name"],
                vec!["group", "room", "wss://a.example", "Duplicate"],
                vec!["group", "room", "wss://b.example", "Fork"],
                vec!["group", "room", "not-a-relay", "Malformed"],
                vec!["r", "wss://used.example"],
                vec!["r", "not-a-relay"],
            ],
            "encrypted private content survives",
        );
        let encoded = [
            operation(GroupListOperation::AddGroup {
                group_id: "room".to_string(),
                host_relay: a.clone(),
                name: Some("New name must not replace old".to_string()),
            }),
            operation(GroupListOperation::AddRelay {
                relay: relay.clone(),
            }),
            operation(GroupListOperation::RemoveGroup {
                group_id: "room".to_string(),
                host_relay: a,
            }),
            operation(GroupListOperation::AddGroup {
                group_id: "new".to_string(),
                host_relay: b.clone(),
                name: None,
            }),
            operation(GroupListOperation::RemoveRelay { relay }),
        ];
        let operations = encoded
            .iter()
            .map(|bytes| operation_ref(bytes))
            .collect::<Vec<_>>();
        let result = apply_operations(&base, &operations).unwrap();
        assert_eq!(result.content, "encrypted private content survives");
        assert_eq!(
            result
                .tags
                .iter()
                .map(|tag| tag.as_slice().to_vec())
                .collect::<Vec<_>>(),
            vec![
                vec!["x".to_string(), "opaque".to_string(), "cells".to_string()],
                vec![
                    "group".to_string(),
                    "room".to_string(),
                    "wss://b.example".to_string(),
                    "Fork".to_string(),
                ],
                vec![
                    "group".to_string(),
                    "room".to_string(),
                    "not-a-relay".to_string(),
                    "Malformed".to_string(),
                ],
                vec!["r".to_string(), "not-a-relay".to_string()],
                vec!["group".to_string(), "new".to_string(), b.to_string(),],
            ]
        );
    }

    #[test]
    fn operation_codec_is_versioned_closed_and_canonicalizes_relays() {
        let operation = GroupListOperation::AddGroup {
            group_id: "room".to_string(),
            host_relay: RelayUrl::parse("wss://relay.example").unwrap(),
            name: Some("Room".to_string()),
        };
        let bytes = encode_operation(&operation).unwrap();
        assert_eq!(decode_operation(&bytes), Ok(operation));
        assert!(decode_operation(
            br#"{"version":2,"action":{"type":"add_relay","relay":"wss://relay.example"}}"#
        )
        .is_err());
        assert!(decode_operation(br#"{"version":1,"extra":true,"action":{"type":"add_relay","relay":"wss://relay.example"}}"#).is_err());
        assert!(decode_operation(
            br#"{"version":1,"action":{"type":"add_relay","relay":"not-a-relay"}}"#
        )
        .is_err());
    }

    #[test]
    fn signed_out_is_refused_and_first_group_enters_ordinary_custody() {
        let engine = Engine::new_with_capabilities(
            crate::EngineConfig::default(),
            vec![group_list_capability()],
        )
        .unwrap();
        let writes = group_list_writes();
        let group = SimpleGroupEntry {
            group_id: "room".to_string(),
            host_relay: RelayUrl::parse("wss://host.example").unwrap(),
            name: None,
        };
        assert_eq!(
            add_group_to_list(&engine, &writes, group.clone()).err(),
            Some(GroupListActionError::SignedOut)
        );
        assert!(engine.publish_queue(None, 10).unwrap().is_empty());

        let author = Keys::generate();
        engine
            .add_private_key_account(&author.secret_key().to_secret_bytes(), true)
            .unwrap();
        let receipt = add_group_to_list(&engine, &writes, group)
            .expect("the capability default enters ordinary custody");
        assert_eq!(
            engine.publish_queue(None, 10).unwrap()[0].receipt_id,
            receipt.id
        );
        engine.shutdown();
    }
}
