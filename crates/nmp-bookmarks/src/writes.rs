//! Durable add/remove operations over the current account's kind:10003
//! public bookmarks list.
//!
//! `nmp` must not know what a bookmark is: this module composes the
//! ordinary [`WriteIntent`](nmp::WriteIntent), freezes the selected
//! account, and enters the engine's receipt lifecycle over `nmp`'s own
//! engine surface -- the same capability-owns-its-meaning shape
//! `nmp-nip02`'s follow door and `nmp-nip29`'s group-list door already use.

use nmp_grammar::{EventBuilder, Identity, WriteIntent, WriteRouting};
use nostr::nips::nip01::Coordinate;
use nostr::{EventId, Kind, RelayUrl, Tag};
use serde::{Deserialize, Serialize};

use nmp::{
    Engine, EngineError, ReceiptStream, RegisteredReplaceableMaterializer, ReplaceableMaterializer,
    ReplaceableMaterializerOperation, ReplaceableMaterializerRefusal, ReplaceableMaterializerSpec,
};

use crate::items::{BookmarkedItem, BOOKMARKS_KIND};

const BOOKMARKS_PROGRAM: [u8; 16] = *b"nmp-bookmarks!!!";
const BOOKMARKS_FORMAT: [u8; 16] = *b"bookmarks-v001!!";
const BOOKMARKS_OPERATION_VERSION: u8 = 1;

/// Compiled bookmarks write composer.
///
/// The handle names only this crate's program/format. Publishing through an
/// engine that was not constructed with [`bookmark_capability`] is refused
/// before custody.
#[derive(Clone, Copy)]
pub struct BookmarkWrites {
    registration: RegisteredReplaceableMaterializer,
}

/// Why a typed bookmark action was refused before ordinary write custody.
/// `EngineClosed` and `PublishRefused` name exactly what
/// [`nmp::Engine::publish`] itself can return for this call
/// ([`EngineError`] has no other reachable variant here); there is no
/// separate bookmarks-only fiction standing in for a receipt that failed to
/// materialize for no named reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BookmarkActionError {
    SignedOut,
    EngineClosed,
    PublishRefused { reason: String },
}

impl std::fmt::Display for BookmarkActionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SignedOut => f.write_str("no current account is selected"),
            Self::EngineClosed => f.write_str("the engine is closed"),
            Self::PublishRefused { reason } => write!(f, "{reason}"),
        }
    }
}

impl std::error::Error for BookmarkActionError {}

/// The compiled bookmarks capability that must be supplied before engine
/// recovery.
#[must_use]
pub fn bookmark_capability() -> ReplaceableMaterializerSpec {
    ReplaceableMaterializerSpec::new(BOOKMARKS_PROGRAM, BOOKMARKS_FORMAT, BookmarksMaterializer)
}

/// Typed bookmarks write constructor bound to [`bookmark_capability`].
#[must_use]
pub fn bookmark_writes() -> BookmarkWrites {
    BookmarkWrites {
        registration: bookmark_capability().handle(),
    }
}

/// Add `item` to the current account's bookmarks, unless an equivalent
/// valid row is already present.
pub fn add_bookmark(
    engine: &Engine,
    writes: &BookmarkWrites,
    item: BookmarkedItem,
) -> Result<ReceiptStream, BookmarkActionError> {
    publish_operation(engine, writes, BookmarkOperation::Add(item))
}

/// Remove every valid row matching `item` from the current account's
/// bookmarks. Malformed near-matches remain byte-for-byte.
pub fn remove_bookmark(
    engine: &Engine,
    writes: &BookmarkWrites,
    item: BookmarkedItem,
) -> Result<ReceiptStream, BookmarkActionError> {
    publish_operation(engine, writes, BookmarkOperation::Remove(item))
}

fn publish_operation(
    engine: &Engine,
    writes: &BookmarkWrites,
    operation: BookmarkOperation,
) -> Result<ReceiptStream, BookmarkActionError> {
    let author = match engine.session() {
        Ok(session) => session
            .current_pubkey
            .ok_or(BookmarkActionError::SignedOut)?,
        Err(_) => return Err(BookmarkActionError::EngineClosed),
    };
    let operation = encode_operation(&operation);
    let payload = writes
        .registration
        .first_value_operation(Kind::Custom(BOOKMARKS_KIND), String::new(), operation)
        .expect(
            "Kind::Custom(10003) with an empty identifier and a fixed non-empty JSON operation \
             is always accepted",
        );
    engine
        .publish(WriteIntent {
            payload,
            routing: WriteRouting::Auto,
            identity: Identity::Explicit(author),
            correlation: None,
        })
        .map_err(|error| match error {
            EngineError::EngineClosed => BookmarkActionError::EngineClosed,
            other => BookmarkActionError::PublishRefused {
                reason: other.to_string(),
            },
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BookmarkOperation {
    Add(BookmarkedItem),
    Remove(BookmarkedItem),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireOperation {
    version: u8,
    action: WireAction,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum WireAction {
    Add { item: WireItem },
    Remove { item: WireItem },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum WireItem {
    Event {
        id: String,
        relay_hint: Option<String>,
    },
    Address {
        coordinate: String,
    },
    Hashtag {
        value: String,
    },
    Url {
        value: String,
    },
}

/// Infallible: `WireOperation`/`WireAction`/`WireItem` are plain structs/
/// enums over `String`/`Option<String>` fields with a derived `Serialize`
/// -- no map with non-string keys, no custom serializer that can fail, no
/// non-finite float.
fn encode_operation(operation: &BookmarkOperation) -> Vec<u8> {
    let action = match operation {
        BookmarkOperation::Add(item) => WireAction::Add {
            item: item_to_wire(item),
        },
        BookmarkOperation::Remove(item) => WireAction::Remove {
            item: item_to_wire(item),
        },
    };
    serde_json::to_vec(&WireOperation {
        version: BOOKMARKS_OPERATION_VERSION,
        action,
    })
    .expect("WireOperation's derived Serialize over String/Option<String> fields cannot fail")
}

fn item_to_wire(item: &BookmarkedItem) -> WireItem {
    match item {
        BookmarkedItem::Event { id, relay_hint } => WireItem::Event {
            id: id.to_hex(),
            relay_hint: relay_hint.as_ref().map(RelayUrl::to_string),
        },
        BookmarkedItem::Address(coordinate) => WireItem::Address {
            coordinate: coordinate.to_string(),
        },
        BookmarkedItem::Hashtag(value) => WireItem::Hashtag {
            value: value.clone(),
        },
        BookmarkedItem::Url(value) => WireItem::Url {
            value: value.clone(),
        },
    }
}

fn decode_operation(bytes: &[u8]) -> Result<BookmarkOperation, String> {
    let wire: WireOperation = serde_json::from_slice(bytes)
        .map_err(|error| format!("bookmarks operation is malformed: {error}"))?;
    if wire.version != BOOKMARKS_OPERATION_VERSION {
        return Err("bookmarks operation has an unsupported version".to_string());
    }
    match wire.action {
        WireAction::Add { item } => Ok(BookmarkOperation::Add(wire_to_item(item)?)),
        WireAction::Remove { item } => Ok(BookmarkOperation::Remove(wire_to_item(item)?)),
    }
}

fn wire_to_item(item: WireItem) -> Result<BookmarkedItem, String> {
    Ok(match item {
        WireItem::Event { id, relay_hint } => BookmarkedItem::Event {
            id: EventId::from_hex(&id)
                .map_err(|_| "bookmarks operation has an invalid event id".to_string())?,
            relay_hint: relay_hint
                .map(|hint| {
                    RelayUrl::parse(&hint)
                        .map_err(|_| "bookmarks operation has an invalid relay hint".to_string())
                })
                .transpose()?,
        },
        WireItem::Address { coordinate } => BookmarkedItem::Address(
            Coordinate::parse(&coordinate)
                .map_err(|_| "bookmarks operation has an invalid coordinate".to_string())?,
        ),
        WireItem::Hashtag { value } => BookmarkedItem::Hashtag(value),
        WireItem::Url { value } => BookmarkedItem::Url(value),
    })
}

struct BookmarksMaterializer;

impl ReplaceableMaterializer for BookmarksMaterializer {
    fn materialize(
        &self,
        source: &nostr::UnsignedEvent,
        current: &nostr::UnsignedEvent,
        operations: &[ReplaceableMaterializerOperation<'_>],
    ) -> Result<EventBuilder, ReplaceableMaterializerRefusal> {
        if source.kind.as_u16() != BOOKMARKS_KIND
            || current.kind.as_u16() != BOOKMARKS_KIND
            || source.pubkey != current.pubkey
            || source.tags.identifier() != current.tags.identifier()
        {
            return Err(ReplaceableMaterializerRefusal {
                reason: "bookmarks materialization source coordinate changed".to_string(),
            });
        }
        apply_operations(current, operations)
    }

    fn materialize_default(
        &self,
        coordinate: &Coordinate,
        operations: &[ReplaceableMaterializerOperation<'_>],
    ) -> Result<EventBuilder, ReplaceableMaterializerRefusal> {
        if coordinate.kind.as_u16() != BOOKMARKS_KIND || !coordinate.identifier.is_empty() {
            return Err(ReplaceableMaterializerRefusal {
                reason: "bookmarks first-value coordinate is not a bookmarks list".to_string(),
            });
        }
        let empty = nostr::UnsignedEvent::new(
            coordinate.public_key,
            nostr::Timestamp::from(0),
            Kind::Custom(BOOKMARKS_KIND),
            Vec::new(),
            String::new(),
        );
        apply_operations(&empty, operations)
    }
}

fn apply_operations(
    current: &nostr::UnsignedEvent,
    operations: &[ReplaceableMaterializerOperation<'_>],
) -> Result<EventBuilder, ReplaceableMaterializerRefusal> {
    let mut tags = current.tags.clone().to_vec();
    for encoded in operations {
        let operation = decode_operation(encoded.bytes())
            .map_err(|reason| ReplaceableMaterializerRefusal { reason })?;
        match operation {
            BookmarkOperation::Add(item) => {
                if !tags.iter().any(|tag| tag_matches_item(tag, &item)) {
                    tags.push(item_to_tag(&item));
                }
            }
            BookmarkOperation::Remove(item) => {
                tags.retain(|tag| !tag_matches_item(tag, &item));
            }
        }
    }
    Ok(EventBuilder {
        kind: Kind::Custom(BOOKMARKS_KIND),
        tags,
        content: current.content.clone(),
        created_at: None,
    })
}

fn item_to_tag(item: &BookmarkedItem) -> Tag {
    match item {
        BookmarkedItem::Event { id, relay_hint } => match relay_hint {
            Some(hint) => Tag::parse(["e", &id.to_hex(), hint.as_str()])
                .expect("a well-formed event id and relay hint form a valid tag"),
            None => Tag::event(*id),
        },
        BookmarkedItem::Address(coordinate) => Tag::coordinate(coordinate.clone(), None),
        BookmarkedItem::Hashtag(value) => Tag::hashtag(value.clone()),
        BookmarkedItem::Url(value) => {
            Tag::parse(["r", value.as_str()]).expect("a URL row is always a valid two-cell tag")
        }
    }
}

/// Whether `tag` is a valid row naming exactly `item`. Relay hints on an
/// EXISTING `e` row are ignored for matching purposes -- two rows naming the
/// same event with different hints are the same bookmark, not two.
fn tag_matches_item(tag: &Tag, item: &BookmarkedItem) -> bool {
    let row = tag.as_slice();
    match item {
        BookmarkedItem::Event { id, .. } => {
            row.first().is_some_and(|cell| cell == "e")
                && row.get(1).is_some_and(|cell| cell == &id.to_hex())
        }
        BookmarkedItem::Address(coordinate) => {
            row.first().is_some_and(|cell| cell == "a")
                && row
                    .get(1)
                    .and_then(|raw| Coordinate::parse(raw).ok())
                    .is_some_and(|parsed| &parsed == coordinate)
        }
        BookmarkedItem::Hashtag(value) => {
            row.first().is_some_and(|cell| cell == "t")
                && row.get(1).is_some_and(|cell| cell == value)
        }
        BookmarkedItem::Url(value) => {
            row.first().is_some_and(|cell| cell == "r")
                && row.get(1).is_some_and(|cell| cell == value)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::Keys;

    fn operation(value: BookmarkOperation) -> Vec<u8> {
        encode_operation(&value)
    }

    fn operation_ref(bytes: &[u8]) -> ReplaceableMaterializerOperation<'_> {
        ReplaceableMaterializerOperation::new(bytes)
    }

    fn current(tags: Vec<Vec<&str>>, content: &str) -> nostr::UnsignedEvent {
        nostr::UnsignedEvent::new(
            Keys::generate().public_key(),
            nostr::Timestamp::from(10),
            Kind::Custom(BOOKMARKS_KIND),
            tags.into_iter()
                .map(|row| Tag::parse(row).unwrap())
                .collect::<Vec<_>>(),
            content,
        )
    }

    #[test]
    fn add_and_remove_operations_touch_only_their_exact_valid_rows() {
        let author = Keys::generate().public_key();
        let event_a = EventId::from_slice(&[1u8; 32]).unwrap();
        let event_b = EventId::from_slice(&[2u8; 32]).unwrap();
        let coordinate = Coordinate::new(Kind::Custom(30_023), author);
        let base = current(
            vec![
                vec!["x", "opaque", "cells"],
                vec!["e", &event_a.to_hex()],
                vec!["e", &event_b.to_hex(), "wss://old.example"],
                vec!["t", "old-tag"],
            ],
            "opaque encrypted private content survives",
        );
        let encoded = [
            operation(BookmarkOperation::Add(BookmarkedItem::Hashtag(
                "new-tag".to_string(),
            ))),
            operation(BookmarkOperation::Remove(BookmarkedItem::Event {
                id: event_a,
                relay_hint: None,
            })),
            operation(BookmarkOperation::Add(BookmarkedItem::Address(
                coordinate.clone(),
            ))),
            operation(BookmarkOperation::Remove(BookmarkedItem::Hashtag(
                "old-tag".to_string(),
            ))),
        ];
        let operations = encoded
            .iter()
            .map(|bytes| operation_ref(bytes))
            .collect::<Vec<_>>();
        let result = apply_operations(&base, &operations).unwrap();
        assert_eq!(result.content, "opaque encrypted private content survives");
        let raw = result
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect::<Vec<_>>();
        assert_eq!(
            raw,
            vec![
                vec!["x".to_string(), "opaque".to_string(), "cells".to_string()],
                vec![
                    "e".to_string(),
                    event_b.to_hex(),
                    "wss://old.example".to_string()
                ],
                vec!["t".to_string(), "new-tag".to_string()],
                vec!["a".to_string(), coordinate.to_string()],
            ]
        );
    }

    #[test]
    fn operation_codec_is_versioned_and_closed() {
        let event_id = EventId::from_slice(&[3u8; 32]).unwrap();
        let op = BookmarkOperation::Add(BookmarkedItem::Event {
            id: event_id,
            relay_hint: Some(RelayUrl::parse("wss://relay.example").unwrap()),
        });
        let bytes = encode_operation(&op);
        assert_eq!(decode_operation(&bytes), Ok(op));

        assert!(decode_operation(
            br#"{"version":2,"action":{"action":"add","item":{"type":"hashtag","value":"x"}}}"#
        )
        .is_err());
        assert!(decode_operation(
            br#"{"version":1,"extra":true,"action":{"action":"add","item":{"type":"hashtag","value":"x"}}}"#
        )
        .is_err());
    }

    #[test]
    fn signed_out_is_refused_and_first_bookmark_enters_ordinary_custody() {
        let engine = Engine::new_with_capabilities(
            nmp::EngineConfig::default(),
            vec![bookmark_capability()],
        )
        .unwrap();
        let writes = bookmark_writes();
        let item = BookmarkedItem::Url("https://example.com".to_string());
        assert_eq!(
            add_bookmark(&engine, &writes, item.clone()).err(),
            Some(BookmarkActionError::SignedOut)
        );
        assert!(engine.publish_queue(None, 10).unwrap().is_empty());

        let author = Keys::generate();
        engine
            .add_private_key_account(&author.secret_key().to_secret_bytes(), true)
            .unwrap();
        let receipt = add_bookmark(&engine, &writes, item)
            .expect("the capability default enters ordinary custody");
        assert_eq!(
            engine.publish_queue(None, 10).unwrap()[0].receipt_id,
            receipt.id
        );
        engine.shutdown();
    }

    #[test]
    fn unregistered_capability_is_refused_with_the_engines_own_reason() {
        let engine = Engine::new_with_capabilities(nmp::EngineConfig::default(), vec![]).unwrap();
        engine
            .add_private_key_account(&Keys::generate().secret_key().to_secret_bytes(), true)
            .unwrap();
        let writes = bookmark_writes();
        assert!(
            matches!(
                add_bookmark(
                    &engine,
                    &writes,
                    BookmarkedItem::Url("https://example.com".to_string())
                ),
                Err(BookmarkActionError::PublishRefused { .. })
            ),
            "an unconfigured bookmarks capability is refused before custody, with the \
             engine's own real refusal reason -- not a bookmarks-only fiction"
        );
        assert!(engine.publish_queue(None, 10).unwrap().is_empty());
        engine.shutdown();
    }
}
