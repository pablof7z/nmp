//! Typed NIP-22 comments over NIP-73 external targets (#572/#822) -- top-level
//! free functions for root-thread demand, decode, and ordinary write-intent
//! composition. None needs an `NmpEngine`: the facade's own [`nmp::nip22`]
//! module owns the schema and pure composition (#851 -- one owner for direct
//! Rust and for this boundary alike), while the generic engine owns the one
//! `FfiWriteIntent -> publish -> receipt` lifecycle.

use nostr::{EventId, PublicKey};

use crate::convert::{
    demand_to_ffi, identity_to_ffi, parse_correlation_token, parse_pubkey, write_payload_to_ffi,
    write_routing_to_ffi, FfiError,
};
use crate::types::{FfiDemand, FfiRow, FfiWriteIntent};

/// A validated NIP-73 external content id (`nmp::nip22::Nip73` mirror).
///
/// `Url` carries the CALLER'S spelling on the way in and the CANONICAL one
/// on the way out: `nmp::nip22::Nip73::url` normalises it (NIP-73's table:
/// *"URL, normalized, no fragment"*), so a native caller who sends
/// `HTTPS://Example.COM/p#x` and then reads the id back sees
/// `https://example.com/p`. Normalising on this side of the boundary too
/// would be a second owner of one rule.
#[derive(uniffi::Enum, Debug, Clone, PartialEq, Eq)]
pub enum FfiNip73 {
    PodcastEpisode { guid: String },
    Url { url: String },
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
        target: FfiNip73,
    },
}

/// What a comment is being written on.
///
/// The two shapes are the two things an app actually holds. `Root` describes
/// an entity by its parts -- what an app has for an external content id, or
/// after decoding a comment. `Row` is an event NMP observed, and its own
/// thread position is read off its own rows, so replying to a deep comment
/// and commenting on a root are the same call: the root cannot be restated
/// wrongly by a caller who thought it knew, which is the correction
/// amethyst#629 needed.
#[derive(uniffi::Enum, Debug, Clone, PartialEq, Eq)]
pub enum FfiCommentTarget {
    Root { root: FfiCommentRoot },
    Row { row: FfiRow },
}

/// A comment's direct parent (`nmp::nip22::CommentParent` mirror).
///
/// DECODE-ONLY since #1243. It used to be half of a composition input, and
/// that was the defect: a caller who states the parent separately from the
/// root can state a pair that never existed together on the wire. Composing
/// now names ONE target ([`FfiCommentTarget`]) and reads the parent from it,
/// so there is no from-FFI direction left to write.
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

fn target_from_ffi(target: FfiNip73) -> Result<nmp::nip22::Nip73, FfiError> {
    let invalid = |err: nmp::nip22::Nip73Error| FfiError::InvalidNip73 {
        reason: err.to_string(),
    };
    match target {
        FfiNip73::PodcastEpisode { guid } => {
            nmp::nip22::Nip73::podcast_episode(&guid).map_err(invalid)
        }
        FfiNip73::Url { url } => nmp::nip22::Nip73::url(&url).map_err(invalid),
        FfiNip73::General { value, kind } => {
            nmp::nip22::Nip73::general(&value, &kind).map_err(invalid)
        }
    }
}

fn target_to_ffi(target: &nmp::nip22::Nip73) -> FfiNip73 {
    match target {
        nmp::nip22::Nip73::PodcastEpisode(guid) => FfiNip73::PodcastEpisode { guid: guid.clone() },
        nmp::nip22::Nip73::Url(url) => FfiNip73::Url { url: url.clone() },
        nmp::nip22::Nip73::General { value, kind } => FfiNip73::General {
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
/// engine-free and stays so: it names no author and reads no clock, because
/// the engine resolves the identity and stamps the time at acceptance.
/// Publish the returned value through
/// [`crate::facade::NmpEngine::publish`], the same generic write lifecycle as
/// every other ordinary intent. `correlation` (#591) passes straight through
/// to `WriteIntent.correlation`; NIP-22 owns no separate correlation,
/// take-once, signing, routing, receipt, or retry machinery.
#[uniffi::export]
pub fn comment_intent(
    target: FfiCommentTarget,
    content: String,
    correlation: Option<String>,
) -> Result<FfiWriteIntent, FfiError> {
    let correlation = correlation
        .as_deref()
        .map(parse_correlation_token)
        .transpose()?;
    let intent = match target {
        FfiCommentTarget::Root { root } => {
            nmp::nip22::comment_intent(&root_from_ffi(root)?, content, correlation)
        }
        FfiCommentTarget::Row { row } => {
            nmp::nip22::comment_intent(&crate::tagging::row_from_ffi(row)?, content, correlation)
        }
    };

    // NIP-22 owns this complete shape, and the FFI layer projects the
    // returned ordinary intent rather than independently re-stating its
    // payload, routing, identity, or correlation policy.
    //
    // #951's bug class is now closed on BOTH axes: every field is projected
    // TOTALLY, so a protocol module that changes which payload it composes
    // or which route it mints crosses this boundary faithfully instead of
    // tripping a closed-contract assertion on an exported path. There is no
    // `unreachable!` left here to trip. The one payload shape with no wire
    // form -- a CAS-guarded replaceable edit, which only ever crosses inside
    // a fused semantic method -- refuses as a typed value.
    let nmp::WriteIntent {
        payload,
        routing,
        identity,
        correlation,
    } = intent;

    Ok(FfiWriteIntent {
        payload: write_payload_to_ffi(payload)?,
        routing: write_routing_to_ffi(routing),
        identity: identity_to_ffi(identity),
        correlation: correlation.map(|token| token.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FfiIdentity, FfiSourceAuthority, FfiWritePayload, FfiWriteRouting};

    fn podcast_root() -> FfiCommentRoot {
        FfiCommentRoot::External {
            target: FfiNip73::PodcastEpisode {
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
        let composed =
            nmp::nip22::compose_comment(&root_from_ffi(podcast_root()).unwrap(), "hi".to_string());
        // The id, author and timestamp the engine would have supplied at
        // acceptance, stated here so a row can exist to decode.
        let created_at = nostr::Timestamp::from(1000u64);
        let tags = nostr::Tags::from_list(composed.tags.clone());
        let row = FfiRow {
            id: nostr::EventId::new(
                &author,
                &created_at,
                &composed.kind,
                &tags,
                &composed.content,
            )
            .to_hex(),
            pubkey: author.to_hex(),
            created_at: created_at.as_secs(),
            kind: composed.kind.as_u16(),
            tags: composed
                .tags
                .iter()
                .map(|t| t.as_slice().to_vec())
                .collect(),
            content: composed.content.clone(),
            sig: "".repeat(64),
            signature_state: crate::types::FfiRowSignatureState::Pending,
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
            signature_state: crate::types::FfiRowSignatureState::Pending,
            sources: vec![],
        };
        let err = decode_comment(row).unwrap_err();
        assert_eq!(err, FfiCommentDecodeError::MissingRoot);
    }

    #[test]
    fn comment_intent_composes_the_ordinary_exact_write_intent() {
        let intent = comment_intent(
            FfiCommentTarget::Root {
                root: podcast_root(),
            },
            "hi".to_string(),
            Some("comment-correlation".to_string()),
        )
        .unwrap();

        let FfiWritePayload::Event { builder } = &intent.payload else {
            panic!("NIP-22 comments must be ordinary builder write intents")
        };
        assert_eq!(builder.created_at, None);
        assert_eq!(builder.kind, 1111);
        assert_eq!(builder.content, "hi");
        let tags = &builder.tags;
        assert_eq!(
            tags,
            &vec![
                vec!["I".to_string(), "podcast:item:guid:guid-1".to_string()],
                vec!["K".to_string(), "podcast:item:guid".to_string()],
                vec!["i".to_string(), "podcast:item:guid:guid-1".to_string()],
                vec!["k".to_string(), "podcast:item:guid".to_string()],
            ]
        );
        assert_eq!(intent.routing, FfiWriteRouting::Auto);
        assert_eq!(intent.identity, FfiIdentity::Active);
        assert_eq!(intent.correlation.as_deref(), Some("comment-correlation"));
    }

    #[test]
    fn composed_comment_uses_the_generic_publish_door() {
        let correlation = "comment-generic-publish".to_string();
        let intent = comment_intent(
            FfiCommentTarget::Root {
                root: podcast_root(),
            },
            "hi".to_string(),
            Some(correlation.clone()),
        )
        .unwrap();
        let engine = crate::facade::NmpEngine::new(crate::facade::NmpEngineConfig {
            nip65: Some(crate::facade::FfiNip65Config {
                indexer_relays: vec!["wss://indexer.example".to_string()],
            }),
            ..crate::facade::NmpEngineConfig::default()
        })
        .expect("engine must build with the provider required by the Auto intent");
        let author = nostr::Keys::generate().public_key();
        engine
            .set_active_account(Some(author.to_hex()))
            .expect("the comment publishes as whoever is active");

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
