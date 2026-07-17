//! Typed NIP-22 comments over NIP-73 external targets (#572) -- top-level
//! free functions for root-thread demand and decode, same shape as
//! [`crate::nip29`]'s precedent (#108/#156): no `NmpEngine` instance is
//! needed for either, since `nmp_nip22`'s default feature set has zero
//! engine dependency (its `comment_intent` takes author/time as EXPLICIT
//! caller parameters, unlike NIP-29's semantic kind:9 message). The
//! take-once composed-intent wrapper is [`crate::nip29::FfiComposedWriteIntent`]
//! itself -- reused verbatim, nothing here is NIP-22-specific about it.

use std::sync::Arc;

use nostr::{EventId, PublicKey};

use crate::convert::{demand_to_ffi, parse_correlation_token, parse_pubkey, FfiError};
use crate::nip29::FfiComposedWriteIntent;
use crate::types::{FfiDemand, FfiRow};

/// A validated NIP-73 external-content target (`nmp_nip22::Nip73Target`
/// mirror).
#[derive(uniffi::Enum, Debug, Clone, PartialEq, Eq)]
pub enum FfiNip73Target {
    PodcastEpisodeGuid { guid: String },
    General { value: String, kind: String },
}

/// The root of a NIP-22 comment thread (`nmp_nip22::CommentRoot` mirror).
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
    },
    External {
        target: FfiNip73Target,
    },
}

/// A comment's direct parent (`nmp_nip22::CommentParent` mirror).
#[derive(uniffi::Enum, Debug, Clone, PartialEq, Eq)]
pub enum FfiCommentParent {
    Root,
    Comment {
        event_id: String,
        author_pubkey: Option<String>,
    },
}

/// A successfully decoded, typed NIP-22 comment (`nmp_nip22::DecodedComment`
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

/// [`decode_comment`]'s typed rejection (`nmp_nip22::CommentDecodeError`
/// mirror). Exhaustive; every variant is constructed by a test
/// (Reachability Gate).
#[derive(uniffi::Error, Debug, Clone, PartialEq, Eq)]
pub enum FfiCommentDecodeError {
    WrongKind { got: u16 },
    MissingRoot,
    DuplicateContradictoryRoot,
    MissingRootKind,
    InvalidRootKind { got: String },
    MalformedRootReference,
    EmptyExternalValue,
    MissingParent,
    DuplicateContradictoryParent,
    MissingParentKind,
    InvalidParentKind { got: String },
    MalformedParentReference,
    ParentDoesNotMatchRootOrComment,
}

impl std::fmt::Display for FfiCommentDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl From<nmp_nip22::CommentDecodeError> for FfiCommentDecodeError {
    fn from(value: nmp_nip22::CommentDecodeError) -> Self {
        match value {
            nmp_nip22::CommentDecodeError::WrongKind { got } => Self::WrongKind { got },
            nmp_nip22::CommentDecodeError::MissingRoot => Self::MissingRoot,
            nmp_nip22::CommentDecodeError::DuplicateContradictoryRoot => {
                Self::DuplicateContradictoryRoot
            }
            nmp_nip22::CommentDecodeError::MissingRootKind => Self::MissingRootKind,
            nmp_nip22::CommentDecodeError::InvalidRootKind { got } => Self::InvalidRootKind { got },
            nmp_nip22::CommentDecodeError::MalformedRootReference => Self::MalformedRootReference,
            nmp_nip22::CommentDecodeError::EmptyExternalValue => Self::EmptyExternalValue,
            nmp_nip22::CommentDecodeError::MissingParent => Self::MissingParent,
            nmp_nip22::CommentDecodeError::DuplicateContradictoryParent => {
                Self::DuplicateContradictoryParent
            }
            nmp_nip22::CommentDecodeError::MissingParentKind => Self::MissingParentKind,
            nmp_nip22::CommentDecodeError::InvalidParentKind { got } => {
                Self::InvalidParentKind { got }
            }
            nmp_nip22::CommentDecodeError::MalformedParentReference => {
                Self::MalformedParentReference
            }
            nmp_nip22::CommentDecodeError::ParentDoesNotMatchRootOrComment => {
                Self::ParentDoesNotMatchRootOrComment
            }
        }
    }
}

fn target_from_ffi(target: FfiNip73Target) -> Result<nmp_nip22::Nip73Target, FfiError> {
    match target {
        FfiNip73Target::PodcastEpisodeGuid { guid } => {
            nmp_nip22::Nip73Target::podcast_episode_guid(&guid).map_err(|err| {
                FfiError::InvalidNip73Target {
                    reason: err.to_string(),
                }
            })
        }
        FfiNip73Target::General { value, kind } => nmp_nip22::Nip73Target::general(&value, &kind)
            .map_err(|err| FfiError::InvalidNip73Target {
                reason: err.to_string(),
            }),
    }
}

fn target_to_ffi(target: &nmp_nip22::Nip73Target) -> FfiNip73Target {
    match target {
        nmp_nip22::Nip73Target::PodcastEpisodeGuid(guid) => {
            FfiNip73Target::PodcastEpisodeGuid { guid: guid.clone() }
        }
        nmp_nip22::Nip73Target::General { value, kind } => FfiNip73Target::General {
            value: value.clone(),
            kind: kind.clone(),
        },
    }
}

fn root_from_ffi(root: FfiCommentRoot) -> Result<nmp_nip22::CommentRoot, FfiError> {
    Ok(match root {
        FfiCommentRoot::Event {
            event_id,
            kind,
            author_pubkey,
        } => nmp_nip22::CommentRoot::Event {
            event_id: EventId::from_hex(&event_id)
                .map_err(|_| FfiError::InvalidEventId { got: event_id })?,
            kind,
            author: author_pubkey.as_deref().map(parse_pubkey).transpose()?,
        },
        FfiCommentRoot::Address {
            author_pubkey,
            kind,
            identifier,
        } => nmp_nip22::CommentRoot::Address {
            author: parse_pubkey(&author_pubkey)?,
            kind,
            identifier,
        },
        FfiCommentRoot::External { target } => {
            nmp_nip22::CommentRoot::External(target_from_ffi(target)?)
        }
    })
}

fn root_to_ffi(root: &nmp_nip22::CommentRoot) -> FfiCommentRoot {
    match root {
        nmp_nip22::CommentRoot::Event {
            event_id,
            kind,
            author,
        } => FfiCommentRoot::Event {
            event_id: event_id.to_hex(),
            kind: *kind,
            author_pubkey: author.map(|pk| pk.to_hex()),
        },
        nmp_nip22::CommentRoot::Address {
            author,
            kind,
            identifier,
        } => FfiCommentRoot::Address {
            author_pubkey: author.to_hex(),
            kind: *kind,
            identifier: identifier.clone(),
        },
        nmp_nip22::CommentRoot::External(target) => FfiCommentRoot::External {
            target: target_to_ffi(target),
        },
    }
}

fn parent_from_ffi(parent: FfiCommentParent) -> Result<nmp_nip22::CommentParent, FfiError> {
    Ok(match parent {
        FfiCommentParent::Root => nmp_nip22::CommentParent::Root,
        FfiCommentParent::Comment {
            event_id,
            author_pubkey,
        } => nmp_nip22::CommentParent::Comment {
            event_id: EventId::from_hex(&event_id)
                .map_err(|_| FfiError::InvalidEventId { got: event_id })?,
            author: author_pubkey.as_deref().map(parse_pubkey).transpose()?,
        },
    })
}

fn parent_to_ffi(parent: &nmp_nip22::CommentParent) -> FfiCommentParent {
    match parent {
        nmp_nip22::CommentParent::Root => FfiCommentParent::Root,
        nmp_nip22::CommentParent::Comment { event_id, author } => FfiCommentParent::Comment {
            event_id: event_id.to_hex(),
            author_pubkey: author.map(|pk| pk.to_hex()),
        },
    }
}

/// The demand for an entire NIP-22 comment thread rooted at `root`:
/// `kinds:[1111]`, scoped by the uppercase root reference on `#I`
/// (`nmp_nip22::comment_thread_demand` mirror).
#[uniffi::export]
pub fn comment_thread_demand(root: FfiCommentRoot) -> Result<FfiDemand, FfiError> {
    let root = root_from_ffi(root)?;
    Ok(demand_to_ffi(nmp_nip22::comment_thread_demand(&root)))
}

/// Decode a delivered kind:1111 [`FfiRow`] into a typed
/// [`FfiDecodedComment`] (`nmp_nip22::decode_comment` mirror). Fallible:
/// malformed or mismatched tag sets stay raw rows, they never become a
/// typed comment.
#[uniffi::export]
pub fn decode_comment(row: FfiRow) -> Result<FfiDecodedComment, FfiCommentDecodeError> {
    let event_id =
        EventId::from_hex(&row.id).map_err(|_| FfiCommentDecodeError::MalformedRootReference)?;
    let author = PublicKey::from_hex(&row.pubkey)
        .map_err(|_| FfiCommentDecodeError::MalformedRootReference)?;
    let decoded = nmp_nip22::decode_comment(
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

/// Compose a durable, author-outbox-routed `WriteIntent` for a NIP-22
/// comment (`nmp_nip22::comment_intent` mirror). Take-once: publish the
/// returned handle through [`crate::facade::NmpEngine::publish_composed`]
/// exactly once. `correlation` (#591) is passed straight through to
/// `WriteIntent.correlation` -- no comment-specific correlation machinery.
#[allow(clippy::too_many_arguments)]
pub(crate) fn comment_intent(
    root: FfiCommentRoot,
    parent: FfiCommentParent,
    author_pubkey: String,
    created_at: u64,
    content: String,
    correlation: Option<String>,
) -> Result<Arc<FfiComposedWriteIntent>, FfiError> {
    let root = root_from_ffi(root)?;
    let parent = parent_from_ffi(parent)?;
    let author = parse_pubkey(&author_pubkey)?;
    let correlation = correlation
        .as_deref()
        .map(parse_correlation_token)
        .transpose()?;
    let intent = nmp_nip22::comment_intent(
        &root,
        parent,
        author,
        nostr::Timestamp::from(created_at),
        content,
        correlation,
    );
    Ok(FfiComposedWriteIntent::new(intent))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FfiSourceAuthority;

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
        let unsigned = nmp_nip22::compose_top_level_comment(
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
    fn comment_intent_composes_a_takeonce_write_intent() {
        let author = nostr::Keys::generate().public_key();
        let intent = comment_intent(
            podcast_root(),
            FfiCommentParent::Root,
            author.to_hex(),
            1000,
            "hi".to_string(),
            None,
        )
        .unwrap();
        let taken = intent.take().unwrap();
        assert!(matches!(
            taken.payload,
            nmp_grammar::WritePayload::Unsigned(_)
        ));
        assert!(
            intent.take().is_err(),
            "take-once must refuse a second take"
        );
    }
}
