//! Typed NIP-22 comments over NIP-73 external targets (#572/#822) -- top-level
//! free functions for root-thread demand, decode, and ordinary write-intent
//! composition. None needs an `NmpEngine`: the facade's own [`nmp::nip22`]
//! module owns the schema and pure composition (#851 -- one owner for direct
//! Rust and for this boundary alike), while the generic engine owns the one
//! `FfiWriteIntent -> publish -> receipt` lifecycle.

use nostr::{EventId, PublicKey};

use crate::convert::{
    demand_to_ffi, parse_correlation_token, parse_pubkey, write_routing_to_ffi, FfiError,
};
use crate::types::{FfiDemand, FfiDurability, FfiRow, FfiWriteIntent, FfiWritePayload};

/// A validated NIP-73 external-content target (`nmp::nip22::Nip73Target`
/// mirror).
#[derive(uniffi::Enum, Debug, Clone, PartialEq, Eq)]
pub enum FfiNip73Target {
    PodcastEpisodeGuid { guid: String },
    General { value: String, kind: String },
}

/// The root of a NIP-22 comment thread (`nmp::nip22::CommentRoot` mirror).
#[derive(uniffi::Enum, Debug, Clone, PartialEq, Eq)]
pub enum FfiCommentRoot {
    Event {
        event_id: String,
        kind: u16,
        author_pubkey: Option<String>,
    },
    Address {
        author_pubkey: String,
        kind: u16,
        identifier: String,
        /// The addressable event's own id, when pinned alongside the
        /// coordinate (NIP-22: "when the parent event is replaceable or
        /// addressable, also include an `e`/`E` tag referencing its id").
        /// `None` remains a fully legal root.
        event_id: Option<String>,
    },
    External {
        target: FfiNip73Target,
    },
}

/// A comment's direct parent (`nmp::nip22::CommentParent` mirror).
#[derive(uniffi::Enum, Debug, Clone, PartialEq, Eq)]
pub enum FfiCommentParent {
    Root,
    Comment {
        event_id: String,
        author_pubkey: Option<String>,
    },
}

/// A successfully decoded, typed NIP-22 comment (`nmp::nip22::DecodedComment`
/// mirror).
#[derive(uniffi::Record, Debug, Clone, PartialEq, Eq)]
pub struct FfiDecodedComment {
    pub event_id: String,
    pub author_pubkey: String,
    pub created_at: u64,
    pub content: String,
    pub root: FfiCommentRoot,
    pub parent: FfiCommentParent,
}

/// [`decode_comment`]'s typed rejection (`nmp::nip22::CommentDecodeError`
/// mirror). Exhaustive; every variant is constructed by a test
/// (Reachability Gate).
#[derive(uniffi::Error, Debug, Clone, PartialEq, Eq)]
pub enum FfiCommentDecodeError {
    WrongKind {
        got: u16,
    },
    MissingRoot,
    DuplicateContradictoryRoot,
    MissingRootKind,
    InvalidRootKind {
        got: String,
    },
    MalformedRootReference,
    EmptyExternalValue,
    MalformedExternalValue {
        got: String,
    },
    MissingParent,
    DuplicateContradictoryParent,
    MissingParentKind,
    InvalidParentKind {
        got: String,
    },
    MalformedParentReference,
    ParentDoesNotMatchRootOrComment,
    /// FFI-boundary-only: the delivered [`FfiRow`]'s OWN `id`/`pubkey`
    /// envelope fields were not valid hex -- distinct from
    /// [`Self::MalformedRootReference`], which describes a root `E`/`A`
    /// TAG reference, never the row's own envelope (#572 review nit: the
    /// two were conflated, misdirecting a caller debugging a bad row id at
    /// the wrong tag).
    MalformedRowEnvelope {
        reason: String,
    },
}

impl std::fmt::Display for FfiCommentDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl From<nmp::nip22::CommentDecodeError> for FfiCommentDecodeError {
    fn from(value: nmp::nip22::CommentDecodeError) -> Self {
        match value {
            nmp::nip22::CommentDecodeError::WrongKind { got } => Self::WrongKind { got },
            nmp::nip22::CommentDecodeError::MissingRoot => Self::MissingRoot,
            nmp::nip22::CommentDecodeError::DuplicateContradictoryRoot => {
                Self::DuplicateContradictoryRoot
            }
            nmp::nip22::CommentDecodeError::MissingRootKind => Self::MissingRootKind,
            nmp::nip22::CommentDecodeError::InvalidRootKind { got } => {
                Self::InvalidRootKind { got }
            }
            nmp::nip22::CommentDecodeError::MalformedRootReference => Self::MalformedRootReference,
            nmp::nip22::CommentDecodeError::EmptyExternalValue => Self::EmptyExternalValue,
            nmp::nip22::CommentDecodeError::MalformedExternalValue { got } => {
                Self::MalformedExternalValue { got }
            }
            nmp::nip22::CommentDecodeError::MissingParent => Self::MissingParent,
            nmp::nip22::CommentDecodeError::DuplicateContradictoryParent => {
                Self::DuplicateContradictoryParent
            }
            nmp::nip22::CommentDecodeError::MissingParentKind => Self::MissingParentKind,
            nmp::nip22::CommentDecodeError::InvalidParentKind { got } => {
                Self::InvalidParentKind { got }
            }
            nmp::nip22::CommentDecodeError::MalformedParentReference => {
                Self::MalformedParentReference
            }
            nmp::nip22::CommentDecodeError::ParentDoesNotMatchRootOrComment => {
                Self::ParentDoesNotMatchRootOrComment
            }
        }
    }
}

fn target_from_ffi(target: FfiNip73Target) -> Result<nmp::nip22::Nip73Target, FfiError> {
    match target {
        FfiNip73Target::PodcastEpisodeGuid { guid } => {
            nmp::nip22::Nip73Target::podcast_episode_guid(&guid).map_err(|err| {
                FfiError::InvalidNip73Target {
                    reason: err.to_string(),
                }
            })
        }
        FfiNip73Target::General { value, kind } => nmp::nip22::Nip73Target::general(&value, &kind)
            .map_err(|err| FfiError::InvalidNip73Target {
                reason: err.to_string(),
            }),
    }
}

fn target_to_ffi(target: &nmp::nip22::Nip73Target) -> FfiNip73Target {
    match target {
        nmp::nip22::Nip73Target::PodcastEpisodeGuid(guid) => {
            FfiNip73Target::PodcastEpisodeGuid { guid: guid.clone() }
        }
        nmp::nip22::Nip73Target::General { value, kind } => FfiNip73Target::General {
            value: value.clone(),
            kind: kind.clone(),
        },
    }
}

fn root_from_ffi(root: FfiCommentRoot) -> Result<nmp::nip22::CommentRoot, FfiError> {
    Ok(match root {
        FfiCommentRoot::Event {
            event_id,
            kind,
            author_pubkey,
        } => nmp::nip22::CommentRoot::Event {
            event_id: EventId::from_hex(&event_id)
                .map_err(|_| FfiError::InvalidEventId { got: event_id })?,
            kind,
            author: author_pubkey.as_deref().map(parse_pubkey).transpose()?,
        },
        FfiCommentRoot::Address {
            author_pubkey,
            kind,
            identifier,
            event_id,
        } => nmp::nip22::CommentRoot::Address {
            author: parse_pubkey(&author_pubkey)?,
            kind,
            identifier,
            event_id: event_id
                .map(|hex| {
                    EventId::from_hex(&hex).map_err(|_| FfiError::InvalidEventId { got: hex })
                })
                .transpose()?,
        },
        FfiCommentRoot::External { target } => {
            nmp::nip22::CommentRoot::External(target_from_ffi(target)?)
        }
    })
}

fn root_to_ffi(root: &nmp::nip22::CommentRoot) -> FfiCommentRoot {
    match root {
        nmp::nip22::CommentRoot::Event {
            event_id,
            kind,
            author,
        } => FfiCommentRoot::Event {
            event_id: event_id.to_hex(),
            kind: *kind,
            author_pubkey: author.map(|pk| pk.to_hex()),
        },
        nmp::nip22::CommentRoot::Address {
            author,
            kind,
            identifier,
            event_id,
        } => FfiCommentRoot::Address {
            author_pubkey: author.to_hex(),
            kind: *kind,
            identifier: identifier.clone(),
            event_id: event_id.map(|id| id.to_hex()),
        },
        nmp::nip22::CommentRoot::External(target) => FfiCommentRoot::External {
            target: target_to_ffi(target),
        },
    }
}

fn parent_from_ffi(parent: FfiCommentParent) -> Result<nmp::nip22::CommentParent, FfiError> {
    Ok(match parent {
        FfiCommentParent::Root => nmp::nip22::CommentParent::Root,
        FfiCommentParent::Comment {
            event_id,
            author_pubkey,
        } => nmp::nip22::CommentParent::Comment {
            event_id: EventId::from_hex(&event_id)
                .map_err(|_| FfiError::InvalidEventId { got: event_id })?,
            author: author_pubkey.as_deref().map(parse_pubkey).transpose()?,
        },
    })
}

fn parent_to_ffi(parent: &nmp::nip22::CommentParent) -> FfiCommentParent {
    match parent {
        nmp::nip22::CommentParent::Root => FfiCommentParent::Root,
        nmp::nip22::CommentParent::Comment { event_id, author } => FfiCommentParent::Comment {
            event_id: event_id.to_hex(),
            author_pubkey: author.map(|pk| pk.to_hex()),
        },
    }
}

/// The demand for an entire NIP-22 comment thread rooted at `root`:
/// `kinds:[1111]`, scoped by the uppercase root reference on `#I`
/// (`nmp::nip22::comment_thread_demand` mirror).
#[uniffi::export]
pub fn comment_thread_demand(root: FfiCommentRoot) -> Result<FfiDemand, FfiError> {
    let root = root_from_ffi(root)?;
    Ok(demand_to_ffi(nmp::nip22::comment_thread_demand(&root)))
}

/// Decode a delivered kind:1111 [`FfiRow`] into a typed
/// [`FfiDecodedComment`] (`nmp::nip22::decode_comment` mirror). Fallible:
/// malformed or mismatched tag sets stay raw rows, they never become a
/// typed comment.
#[uniffi::export]
pub fn decode_comment(row: FfiRow) -> Result<FfiDecodedComment, FfiCommentDecodeError> {
    let event_id =
        EventId::from_hex(&row.id).map_err(|_| FfiCommentDecodeError::MalformedRowEnvelope {
            reason: format!("row.id is not valid event-id hex: {}", row.id),
        })?;
    let author = PublicKey::from_hex(&row.pubkey).map_err(|_| {
        FfiCommentDecodeError::MalformedRowEnvelope {
            reason: format!("row.pubkey is not valid public-key hex: {}", row.pubkey),
        }
    })?;
    let decoded = nmp::nip22::decode_comment(
        event_id,
        author,
        row.created_at,
        row.kind,
        &row.tags,
        &row.content,
    )?;
    Ok(FfiDecodedComment {
        event_id: decoded.event_id.to_hex(),
        author_pubkey: decoded.author.to_hex(),
        created_at: decoded.created_at,
        content: decoded.content,
        root: root_to_ffi(&decoded.root),
        parent: parent_to_ffi(&decoded.parent),
    })
}

/// Compose an ordinary durable, `Auto`-routed [`FfiWriteIntent`] for a
/// NIP-22 comment (`nmp::nip22::comment_intent` mirror). This function is
/// engine-free: author and event time are explicit deterministic composition
/// inputs. Publish the returned value through
/// [`crate::facade::NmpEngine::publish`], the same generic write lifecycle as
/// every other ordinary intent. `correlation` (#591) passes straight through
/// to `WriteIntent.correlation`; NIP-22 owns no separate correlation,
/// take-once, signing, routing, receipt, or retry machinery.
#[uniffi::export]
#[allow(clippy::too_many_arguments)]
pub fn comment_intent(
    root: FfiCommentRoot,
    parent: FfiCommentParent,
    author_pubkey: String,
    created_at: u64,
    content: String,
    correlation: Option<String>,
) -> Result<FfiWriteIntent, FfiError> {
    let root = root_from_ffi(root)?;
    let parent = parent_from_ffi(parent)?;
    let author = parse_pubkey(&author_pubkey)?;
    let correlation = correlation
        .as_deref()
        .map(parse_correlation_token)
        .transpose()?;
    let intent = nmp::nip22::comment_intent(
        &root,
        parent,
        author,
        nostr::Timestamp::from(created_at),
        content,
        correlation,
    );

    // NIP-22 owns this complete shape. The FFI layer projects the returned
    // ordinary intent instead of independently re-stating its payload,
    // durability, routing, identity, or correlation policy.
    //
    // Routing is deliberately NOT part of the closed-contract pattern below:
    // it is projected totally (`write_routing_to_ffi`), so a protocol module
    // that changes which route it mints crosses this boundary faithfully
    // rather than panicking on an exported path. Every routing value has a
    // wire form now, so there is nothing left for routing to drift into.
    let nmp::WriteIntent {
        payload: nmp::WritePayload::Unsigned(unsigned),
        durability: nmp::Durability::Durable,
        routing,
        identity_override: None,
        correlation,
    } = intent
    else {
        unreachable!("nmp::nip22::comment_intent violated its closed write contract")
    };
    let routing = write_routing_to_ffi(routing);

    Ok(FfiWriteIntent {
        payload: FfiWritePayload::Unsigned {
            pubkey: unsigned.pubkey.to_hex(),
            created_at: unsigned.created_at.as_secs(),
            kind: unsigned.kind.as_u16(),
            tags: unsigned
                .tags
                .iter()
                .map(|tag| tag.as_slice().to_vec())
                .collect(),
            content: unsigned.content,
        },
        durability: FfiDurability::Durable,
        routing,
        identity_override: None,
        correlation: correlation.map(|token| token.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FfiSourceAuthority, FfiWriteRouting};

    fn podcast_root() -> FfiCommentRoot {
        FfiCommentRoot::External {
            target: FfiNip73Target::PodcastEpisodeGuid {
                guid: "guid-1".to_string(),
            },
        }
    }

    #[test]
    fn comment_thread_demand_scopes_kind_1111() {
        let demand = comment_thread_demand(podcast_root()).unwrap();
        assert_eq!(demand.selection.kinds, Some(vec![1111]));
        assert!(matches!(demand.source, FfiSourceAuthority::Public));
    }

    #[test]
    fn decode_comment_round_trips_a_valid_top_level_comment() {
        let author = nostr::Keys::generate().public_key();
        let unsigned = nmp::nip22::compose_top_level_comment(
            &root_from_ffi(podcast_root()).unwrap(),
            author,
            nostr::Timestamp::from(1000u64),
            "hi".to_string(),
        );
        let row = FfiRow {
            id: unsigned.id.unwrap().to_hex(),
            pubkey: unsigned.pubkey.to_hex(),
            created_at: unsigned.created_at.as_secs(),
            kind: unsigned.kind.as_u16(),
            tags: unsigned
                .tags
                .iter()
                .map(|t| t.as_slice().to_vec())
                .collect(),
            content: unsigned.content.clone(),
            sig: "".repeat(64),
            sources: vec![],
        };
        let decoded = decode_comment(row).expect("valid comment must decode");
        assert_eq!(decoded.root, podcast_root());
        assert_eq!(decoded.parent, FfiCommentParent::Root);
    }

    #[test]
    fn decode_comment_rejects_missing_root() {
        let row = FfiRow {
            id: EventId::from_slice(&[1; 32]).unwrap().to_hex(),
            pubkey: nostr::Keys::generate().public_key().to_hex(),
            created_at: 1000,
            kind: 1111,
            tags: vec![],
            content: String::new(),
            sig: "".repeat(64),
            sources: vec![],
        };
        let err = decode_comment(row).unwrap_err();
        assert_eq!(err, FfiCommentDecodeError::MissingRoot);
    }

    #[test]
    fn comment_intent_composes_the_ordinary_exact_write_intent() {
        let author = nostr::Keys::generate().public_key();
        let intent = comment_intent(
            podcast_root(),
            FfiCommentParent::Root,
            author.to_hex(),
            1000,
            "hi".to_string(),
            Some("comment-correlation".to_string()),
        )
        .unwrap();

        let FfiWritePayload::Unsigned {
            pubkey,
            created_at,
            kind,
            tags,
            content,
        } = &intent.payload
        else {
            panic!("NIP-22 comments must be ordinary unsigned write intents")
        };
        assert_eq!(pubkey, &author.to_hex());
        assert_eq!(*created_at, 1000);
        assert_eq!(*kind, 1111);
        assert_eq!(content, "hi");
        assert_eq!(
            tags,
            &vec![
                vec!["I".to_string(), "podcast:item:guid:guid-1".to_string()],
                vec!["K".to_string(), "podcast:item:guid".to_string()],
                vec!["i".to_string(), "podcast:item:guid:guid-1".to_string()],
                vec!["k".to_string(), "podcast:item:guid".to_string()],
            ]
        );
        assert_eq!(intent.durability, FfiDurability::Durable);
        assert_eq!(intent.routing, FfiWriteRouting::Auto);
        assert_eq!(intent.identity_override, None);
        assert_eq!(intent.correlation.as_deref(), Some("comment-correlation"));
    }

    #[test]
    fn composed_comment_uses_the_generic_publish_door() {
        let author = nostr::Keys::generate().public_key();
        let correlation = "comment-generic-publish".to_string();
        let intent = comment_intent(
            podcast_root(),
            FfiCommentParent::Root,
            author.to_hex(),
            1000,
            "hi".to_string(),
            Some(correlation.clone()),
        )
        .unwrap();
        let engine = crate::facade::NmpEngine::new(crate::facade::NmpEngineConfig::default())
            .expect("engine must build");
        engine
            .set_active_account(Some(author.to_hex()))
            .expect("the composed author must be active");

        let receipt = engine
            .publish(intent)
            .expect("the ordinary generic publish door must accept the comment");
        let receipt_id = receipt.id();
        let reattached = engine
            .reattach_by_correlation(correlation)
            .expect("the generic door must preserve the comment correlation token");
        assert_eq!(reattached.receipt_id, Some(receipt_id));
        match reattached.outcome {
            crate::types::FfiReceiptReattachment::Attached { stream } => {
                assert_eq!(stream.id(), receipt_id);
            }
            crate::types::FfiReceiptReattachment::NotFound => {
                panic!("the accepted comment correlation must be retained")
            }
            crate::types::FfiReceiptReattachment::RetainedButUnreadable => {
                panic!("the accepted comment receipt must remain readable")
            }
        }
        engine.shutdown();
    }
}
