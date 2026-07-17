//! Fallible NIP-22 kind:1111 decode (#572). Malformed or mismatched tag
//! sets are TYPED REJECTIONS -- they stay raw rows, they never become a
//! [`DecodedComment`]. Unlike `nmp_nip68::decode_picture`'s tolerant
//! decode (diagnostics attached to a still-returned value), a NIP-22
//! comment's root/parent identity is load-bearing for thread placement, so
//! there is no "decode with diagnostics" middle ground here -- it decodes
//! completely or it is rejected completely.

use nostr::{EventId, PublicKey};

use crate::root::{CommentParent, CommentRoot, COMMENT_KIND};
use crate::target::Nip73Target;

/// A successfully decoded, typed NIP-22 comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedComment {
    pub event_id: EventId,
    pub author: PublicKey,
    pub created_at: u64,
    pub content: String,
    pub root: CommentRoot,
    pub parent: CommentParent,
}

/// [`decode_comment`]'s typed rejection. Exhaustive; every variant is
/// constructed by a test (Reachability Gate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommentDecodeError {
    /// The event's `kind` was not 1111.
    WrongKind { got: u16 },
    /// No root tag (`E`/`A`/`I`) was present at all.
    MissingRoot,
    /// More than one DISTINCT root tag type (`E`/`A`/`I`) was present --
    /// a duplicate, contradictory root claim.
    DuplicateContradictoryRoot,
    /// A root tag was present but its required `K` companion was missing.
    MissingRootKind,
    /// The root `K` value did not parse as a valid kind number (`E`/`A`
    /// roots only -- `I` roots carry an opaque `K` string, never a kind
    /// number).
    InvalidRootKind { got: String },
    /// A root `E`/`A` reference did not parse (bad event id hex, or a
    /// malformed `<kind>:<pubkey>:<d>` address coordinate).
    MalformedRootReference,
    /// An `I`/`i` or `K`/`k` cell was the empty string.
    EmptyExternalValue,
    /// No parent tag (`e`/`a`/`i`) was present at all.
    MissingParent,
    /// More than one distinct parent tag type was present.
    DuplicateContradictoryParent,
    /// A parent tag was present but its required `k` companion was
    /// missing.
    MissingParentKind,
    /// The parent `k` value did not parse as a valid kind number.
    InvalidParentKind { got: String },
    /// A parent `e`/`a` reference did not parse.
    MalformedParentReference,
    /// The parent tag neither exactly mirrors the root (a valid top-level
    /// comment) nor is a well-formed `e` + `k=1111` comment-parent
    /// reference (a valid reply). Covers "mismatched I/i", "wrong
    /// external kind", and any other root/parent shape disagreement.
    ParentDoesNotMatchRootOrComment,
}

impl std::fmt::Display for CommentDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongKind { got } => write!(f, "expected kind 1111, got {got}"),
            Self::MissingRoot => f.write_str("no root (E/A/I) tag present"),
            Self::DuplicateContradictoryRoot => {
                f.write_str("more than one distinct root (E/A/I) tag present")
            }
            Self::MissingRootKind => f.write_str("root tag present without its required K"),
            Self::InvalidRootKind { got } => write!(f, "root K {got:?} is not a valid kind number"),
            Self::MalformedRootReference => f.write_str("root E/A reference did not parse"),
            Self::EmptyExternalValue => f.write_str("I/i or K/k cell was empty"),
            Self::MissingParent => f.write_str("no parent (e/a/i) tag present"),
            Self::DuplicateContradictoryParent => {
                f.write_str("more than one distinct parent (e/a/i) tag present")
            }
            Self::MissingParentKind => f.write_str("parent tag present without its required k"),
            Self::InvalidParentKind { got } => {
                write!(f, "parent k {got:?} is not a valid kind number")
            }
            Self::MalformedParentReference => f.write_str("parent e/a reference did not parse"),
            Self::ParentDoesNotMatchRootOrComment => f.write_str(
                "parent tag neither mirrors the root nor is a valid e+k=1111 comment reference",
            ),
        }
    }
}

impl std::error::Error for CommentDecodeError {}

/// One decoded root candidate, before cross-validation against the parent.
enum RawRoot {
    Event {
        event_id: EventId,
        kind_tag: String,
        author: Option<PublicKey>,
    },
    Address {
        coordinate: String,
        kind_tag: String,
    },
    External {
        i_value: String,
        k_value: String,
    },
}

fn find_tag<'a>(tags: &'a [Vec<String>], name: &str) -> Option<&'a [String]> {
    tags.iter()
        .find(|tag| tag.first().map(String::as_str) == Some(name))
        .map(Vec::as_slice)
}

fn count_present(tags: &[Vec<String>], names: &[&str]) -> usize {
    names
        .iter()
        .filter(|name| find_tag(tags, name).is_some())
        .count()
}

fn parse_address_coordinate(coordinate: &str) -> Option<(u16, PublicKey, String)> {
    let mut parts = coordinate.splitn(3, ':');
    let kind = parts.next()?.parse::<u16>().ok()?;
    let pubkey = PublicKey::from_hex(parts.next()?).ok()?;
    let identifier = parts.next()?.to_string();
    Some((kind, pubkey, identifier))
}

fn decode_root(tags: &[Vec<String>]) -> Result<RawRoot, CommentDecodeError> {
    let present = count_present(tags, &["E", "A", "I"]);
    if present == 0 {
        return Err(CommentDecodeError::MissingRoot);
    }
    if present > 1 {
        return Err(CommentDecodeError::DuplicateContradictoryRoot);
    }

    if let Some(e) = find_tag(tags, "E") {
        let event_id = e
            .get(1)
            .and_then(|hex| EventId::from_hex(hex).ok())
            .ok_or(CommentDecodeError::MalformedRootReference)?;
        let kind_tag = find_tag(tags, "K")
            .and_then(|k| k.get(1))
            .cloned()
            .ok_or(CommentDecodeError::MissingRootKind)?;
        let author = find_tag(tags, "P")
            .and_then(|p| p.get(1))
            .and_then(|hex| PublicKey::from_hex(hex).ok());
        return Ok(RawRoot::Event {
            event_id,
            kind_tag,
            author,
        });
    }

    if let Some(a) = find_tag(tags, "A") {
        let coordinate = a
            .get(1)
            .cloned()
            .ok_or(CommentDecodeError::MalformedRootReference)?;
        if parse_address_coordinate(&coordinate).is_none() {
            return Err(CommentDecodeError::MalformedRootReference);
        }
        let kind_tag = find_tag(tags, "K")
            .and_then(|k| k.get(1))
            .cloned()
            .ok_or(CommentDecodeError::MissingRootKind)?;
        return Ok(RawRoot::Address {
            coordinate,
            kind_tag,
        });
    }

    // Must be "I" -- the only remaining case given `present == 1`.
    let i = find_tag(tags, "I").expect("I present: only remaining root tag type");
    let i_value = i
        .get(1)
        .cloned()
        .filter(|v| !v.is_empty())
        .ok_or(CommentDecodeError::EmptyExternalValue)?;
    let k_value = find_tag(tags, "K")
        .and_then(|k| k.get(1))
        .cloned()
        .ok_or(CommentDecodeError::MissingRootKind)?;
    if k_value.is_empty() {
        return Err(CommentDecodeError::EmptyExternalValue);
    }
    Ok(RawRoot::External { i_value, k_value })
}

enum RawParent {
    Event {
        event_id: EventId,
        kind_tag: String,
        author: Option<PublicKey>,
    },
    Address {
        coordinate: String,
        kind_tag: String,
    },
    External {
        i_value: String,
        k_value: String,
    },
}

fn decode_parent(tags: &[Vec<String>]) -> Result<RawParent, CommentDecodeError> {
    let present = count_present(tags, &["e", "a", "i"]);
    if present == 0 {
        return Err(CommentDecodeError::MissingParent);
    }
    if present > 1 {
        return Err(CommentDecodeError::DuplicateContradictoryParent);
    }

    if let Some(e) = find_tag(tags, "e") {
        let event_id = e
            .get(1)
            .and_then(|hex| EventId::from_hex(hex).ok())
            .ok_or(CommentDecodeError::MalformedParentReference)?;
        let kind_tag = find_tag(tags, "k")
            .and_then(|k| k.get(1))
            .cloned()
            .ok_or(CommentDecodeError::MissingParentKind)?;
        let author = find_tag(tags, "p")
            .and_then(|p| p.get(1))
            .and_then(|hex| PublicKey::from_hex(hex).ok());
        return Ok(RawParent::Event {
            event_id,
            kind_tag,
            author,
        });
    }

    if let Some(a) = find_tag(tags, "a") {
        let coordinate = a
            .get(1)
            .cloned()
            .ok_or(CommentDecodeError::MalformedParentReference)?;
        if parse_address_coordinate(&coordinate).is_none() {
            return Err(CommentDecodeError::MalformedParentReference);
        }
        let kind_tag = find_tag(tags, "k")
            .and_then(|k| k.get(1))
            .cloned()
            .ok_or(CommentDecodeError::MissingParentKind)?;
        return Ok(RawParent::Address {
            coordinate,
            kind_tag,
        });
    }

    let i = find_tag(tags, "i").expect("i present: only remaining parent tag type");
    let i_value = i
        .get(1)
        .cloned()
        .filter(|v| !v.is_empty())
        .ok_or(CommentDecodeError::EmptyExternalValue)?;
    let k_value = find_tag(tags, "k")
        .and_then(|k| k.get(1))
        .cloned()
        .ok_or(CommentDecodeError::MissingParentKind)?;
    if k_value.is_empty() {
        return Err(CommentDecodeError::EmptyExternalValue);
    }
    Ok(RawParent::External { i_value, k_value })
}

fn root_to_typed(raw: RawRoot) -> Result<CommentRoot, CommentDecodeError> {
    match raw {
        RawRoot::Event {
            event_id,
            kind_tag,
            author,
        } => {
            let kind = kind_tag
                .parse::<u16>()
                .map_err(|_| CommentDecodeError::InvalidRootKind { got: kind_tag })?;
            Ok(CommentRoot::Event {
                event_id,
                kind,
                author,
            })
        }
        RawRoot::Address {
            coordinate,
            kind_tag,
        } => {
            let (coord_kind, author, identifier) =
                parse_address_coordinate(&coordinate).expect("already validated in decode_root");
            let kind = kind_tag
                .parse::<u16>()
                .map_err(|_| CommentDecodeError::InvalidRootKind { got: kind_tag })?;
            if kind != coord_kind {
                return Err(CommentDecodeError::MalformedRootReference);
            }
            Ok(CommentRoot::Address {
                author,
                kind,
                identifier,
            })
        }
        RawRoot::External { i_value, k_value } => {
            let target = if k_value == Nip73Target::PODCAST_EPISODE_GUID_KIND {
                Nip73Target::podcast_episode_guid(&i_value)
            } else {
                Nip73Target::general(&i_value, &k_value)
            }
            .map_err(|_| CommentDecodeError::EmptyExternalValue)?;
            Ok(CommentRoot::External(target))
        }
    }
}

/// Cross-validate the decoded parent against the decoded root, producing
/// the typed [`CommentParent`]. A parent is legal iff it EXACTLY mirrors
/// the root (top-level) or is an `e` + `k=1111` comment reference (reply).
fn parent_to_typed(
    root: &CommentRoot,
    raw: RawParent,
) -> Result<CommentParent, CommentDecodeError> {
    match (&root, &raw) {
        (
            CommentRoot::Event { event_id, kind, .. },
            RawParent::Event {
                event_id: pe,
                kind_tag,
                author,
            },
        ) => {
            let parent_kind =
                kind_tag
                    .parse::<u16>()
                    .map_err(|_| CommentDecodeError::InvalidParentKind {
                        got: kind_tag.clone(),
                    })?;
            if pe == event_id && parent_kind == *kind {
                Ok(CommentParent::Root)
            } else if parent_kind == COMMENT_KIND {
                Ok(CommentParent::Comment {
                    event_id: *pe,
                    author: *author,
                })
            } else {
                Err(CommentDecodeError::ParentDoesNotMatchRootOrComment)
            }
        }
        (
            _,
            RawParent::Event {
                event_id: pe,
                kind_tag,
                author,
            },
        ) => {
            // Root is Address or External: the ONLY legal `e`-shaped
            // parent is a comment reference (kind 1111); an `e` parent can
            // never mirror a non-Event root.
            let parent_kind =
                kind_tag
                    .parse::<u16>()
                    .map_err(|_| CommentDecodeError::InvalidParentKind {
                        got: kind_tag.clone(),
                    })?;
            if parent_kind == COMMENT_KIND {
                Ok(CommentParent::Comment {
                    event_id: *pe,
                    author: *author,
                })
            } else {
                Err(CommentDecodeError::ParentDoesNotMatchRootOrComment)
            }
        }
        (
            CommentRoot::Address {
                author: root_author,
                kind,
                identifier,
            },
            RawParent::Address {
                coordinate,
                kind_tag,
                ..
            },
        ) => {
            let parent_kind =
                kind_tag
                    .parse::<u16>()
                    .map_err(|_| CommentDecodeError::InvalidParentKind {
                        got: kind_tag.clone(),
                    })?;
            let expected = CommentRoot::address_coordinate(*kind, root_author, identifier);
            if *coordinate == expected && parent_kind == *kind {
                Ok(CommentParent::Root)
            } else {
                Err(CommentDecodeError::ParentDoesNotMatchRootOrComment)
            }
        }
        (CommentRoot::External(target), RawParent::External { i_value, k_value }) => {
            if i_value == target.i_value() && k_value == target.k_value() {
                Ok(CommentParent::Root)
            } else {
                Err(CommentDecodeError::ParentDoesNotMatchRootOrComment)
            }
        }
        _ => Err(CommentDecodeError::ParentDoesNotMatchRootOrComment),
    }
}

/// Decode a kind:1111 event's raw fields into a typed [`DecodedComment`].
/// Fallible: malformed or mismatched tag sets return a typed
/// [`CommentDecodeError`] and never become a typed comment.
pub fn decode_comment(
    event_id: EventId,
    author: PublicKey,
    created_at: u64,
    kind: u16,
    tags: &[Vec<String>],
    content: &str,
) -> Result<DecodedComment, CommentDecodeError> {
    if kind != COMMENT_KIND {
        return Err(CommentDecodeError::WrongKind { got: kind });
    }
    let raw_root = decode_root(tags)?;
    let raw_parent = decode_parent(tags)?;
    let root = root_to_typed(raw_root)?;
    let parent = parent_to_typed(&root, raw_parent)?;
    Ok(DecodedComment {
        event_id,
        author,
        created_at,
        content: content.to_string(),
        root,
        parent,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::{compose_comment_reply, compose_top_level_comment};
    use nostr::{Keys, Timestamp};

    fn keys() -> Keys {
        Keys::generate()
    }

    fn row(cells: &[&str]) -> Vec<String> {
        cells.iter().map(|c| c.to_string()).collect()
    }

    fn decode_unsigned(
        unsigned: &nostr::UnsignedEvent,
    ) -> Result<DecodedComment, CommentDecodeError> {
        let rows: Vec<Vec<String>> = unsigned
            .tags
            .iter()
            .map(|t| t.as_slice().to_vec())
            .collect();
        decode_comment(
            unsigned.id.expect("unsigned event has a computed id"),
            unsigned.pubkey,
            unsigned.created_at.as_secs(),
            unsigned.kind.as_u16(),
            &rows,
            &unsigned.content,
        )
    }

    /// A valid top-level podcast comment decodes.
    #[test]
    fn valid_top_level_podcast_comment_decodes() {
        let root = CommentRoot::External(Nip73Target::podcast_episode_guid("guid-1").unwrap());
        let author = keys().public_key();
        let unsigned =
            compose_top_level_comment(&root, author, Timestamp::from(1000u64), "hi".to_string());
        let decoded = decode_unsigned(&unsigned).expect("valid top-level comment must decode");
        assert_eq!(decoded.root, root);
        assert_eq!(decoded.parent, CommentParent::Root);
        assert_eq!(decoded.author, author);
        assert_eq!(decoded.content, "hi");
    }

    /// A valid reply retains the podcast root and exposes its comment
    /// parent.
    #[test]
    fn valid_reply_retains_podcast_root_and_exposes_comment_parent() {
        let root = CommentRoot::External(Nip73Target::podcast_episode_guid("guid-1").unwrap());
        let parent_author = keys().public_key();
        let parent_id = EventId::from_slice(&[1; 32]).unwrap();
        let author = keys().public_key();
        let unsigned = compose_comment_reply(
            &root,
            CommentParent::Comment {
                event_id: parent_id,
                author: Some(parent_author),
            },
            author,
            Timestamp::from(1001u64),
            "reply".to_string(),
        );
        let decoded = decode_unsigned(&unsigned).expect("valid reply must decode");
        assert_eq!(decoded.root, root);
        assert_eq!(
            decoded.parent,
            CommentParent::Comment {
                event_id: parent_id,
                author: Some(parent_author)
            }
        );
    }

    /// Missing K on an external root is rejected.
    #[test]
    fn missing_root_kind_is_rejected() {
        let tags = vec![
            row(&["I", "guid-1"]),
            row(&["i", "guid-1"]),
            row(&["k", "podcast:item:guid"]),
        ];
        let err = decode_comment(
            EventId::from_slice(&[2; 32]).unwrap(),
            keys().public_key(),
            1000,
            COMMENT_KIND,
            &tags,
            "",
        )
        .unwrap_err();
        assert_eq!(err, CommentDecodeError::MissingRootKind);
    }

    /// Missing k on the parent is rejected.
    #[test]
    fn missing_parent_kind_is_rejected() {
        let tags = vec![
            row(&["I", "guid-1"]),
            row(&["K", "podcast:item:guid"]),
            row(&["i", "guid-1"]),
        ];
        let err = decode_comment(
            EventId::from_slice(&[3; 32]).unwrap(),
            keys().public_key(),
            1000,
            COMMENT_KIND,
            &tags,
            "",
        )
        .unwrap_err();
        assert_eq!(err, CommentDecodeError::MissingParentKind);
    }

    /// Mismatched I/i (a top-level parent claiming a DIFFERENT external
    /// value than the root) is rejected.
    #[test]
    fn mismatched_i_and_lowercase_i_is_rejected() {
        let tags = vec![
            row(&["I", "guid-1"]),
            row(&["K", "podcast:item:guid"]),
            row(&["i", "guid-DIFFERENT"]),
            row(&["k", "podcast:item:guid"]),
        ];
        let err = decode_comment(
            EventId::from_slice(&[4; 32]).unwrap(),
            keys().public_key(),
            1000,
            COMMENT_KIND,
            &tags,
            "",
        )
        .unwrap_err();
        assert_eq!(err, CommentDecodeError::ParentDoesNotMatchRootOrComment);
    }

    /// A wrong external kind (parent's k doesn't match root's K, even
    /// though the value matches) is rejected.
    #[test]
    fn wrong_external_kind_is_rejected() {
        let tags = vec![
            row(&["I", "guid-1"]),
            row(&["K", "podcast:item:guid"]),
            row(&["i", "guid-1"]),
            row(&["k", "some-other-namespace"]),
        ];
        let err = decode_comment(
            EventId::from_slice(&[5; 32]).unwrap(),
            keys().public_key(),
            1000,
            COMMENT_KIND,
            &tags,
            "",
        )
        .unwrap_err();
        assert_eq!(err, CommentDecodeError::ParentDoesNotMatchRootOrComment);
    }

    /// Duplicate contradictory root tags (both E and I present) are
    /// rejected.
    #[test]
    fn duplicate_contradictory_root_tags_are_rejected() {
        let tags = vec![
            row(&["E", &EventId::from_slice(&[6; 32]).unwrap().to_hex()]),
            row(&["I", "guid-1"]),
            row(&["K", "podcast:item:guid"]),
            row(&["i", "guid-1"]),
            row(&["k", "podcast:item:guid"]),
        ];
        let err = decode_comment(
            EventId::from_slice(&[7; 32]).unwrap(),
            keys().public_key(),
            1000,
            COMMENT_KIND,
            &tags,
            "",
        )
        .unwrap_err();
        assert_eq!(err, CommentDecodeError::DuplicateContradictoryRoot);
    }

    /// A malformed event reference (bad hex) is rejected.
    #[test]
    fn malformed_event_reference_is_rejected() {
        let tags = vec![
            row(&["E", "not-valid-hex"]),
            row(&["K", "1"]),
            row(&["e", "not-valid-hex"]),
            row(&["k", "1"]),
        ];
        let err = decode_comment(
            EventId::from_slice(&[8; 32]).unwrap(),
            keys().public_key(),
            1000,
            COMMENT_KIND,
            &tags,
            "",
        )
        .unwrap_err();
        assert_eq!(err, CommentDecodeError::MalformedRootReference);
    }

    /// An unrelated target (no NIP-22 root/parent tags at all) never
    /// becomes a typed comment.
    #[test]
    fn unrelated_event_with_no_root_or_parent_tags_is_rejected() {
        let tags = vec![row(&["t", "podcast"])];
        let err = decode_comment(
            EventId::from_slice(&[9; 32]).unwrap(),
            keys().public_key(),
            1000,
            COMMENT_KIND,
            &tags,
            "",
        )
        .unwrap_err();
        assert_eq!(err, CommentDecodeError::MissingRoot);
    }

    /// Wrong kind (not 1111) is rejected before any tag is even examined.
    #[test]
    fn wrong_kind_is_rejected() {
        let err = decode_comment(
            EventId::from_slice(&[10; 32]).unwrap(),
            keys().public_key(),
            1000,
            1,
            &[],
            "",
        )
        .unwrap_err();
        assert_eq!(err, CommentDecodeError::WrongKind { got: 1 });
    }

    /// Wildcard-free exhaustiveness witness over `CommentDecodeError` --
    /// adding a variant without updating this match breaks the build.
    #[test]
    fn comment_decode_error_is_exhaustive() {
        let variants = [
            CommentDecodeError::WrongKind { got: 0 },
            CommentDecodeError::MissingRoot,
            CommentDecodeError::DuplicateContradictoryRoot,
            CommentDecodeError::MissingRootKind,
            CommentDecodeError::InvalidRootKind { got: String::new() },
            CommentDecodeError::MalformedRootReference,
            CommentDecodeError::EmptyExternalValue,
            CommentDecodeError::MissingParent,
            CommentDecodeError::DuplicateContradictoryParent,
            CommentDecodeError::MissingParentKind,
            CommentDecodeError::InvalidParentKind { got: String::new() },
            CommentDecodeError::MalformedParentReference,
            CommentDecodeError::ParentDoesNotMatchRootOrComment,
        ];
        for variant in &variants {
            let described = match variant {
                CommentDecodeError::WrongKind { .. }
                | CommentDecodeError::MissingRoot
                | CommentDecodeError::DuplicateContradictoryRoot
                | CommentDecodeError::MissingRootKind
                | CommentDecodeError::InvalidRootKind { .. }
                | CommentDecodeError::MalformedRootReference
                | CommentDecodeError::EmptyExternalValue
                | CommentDecodeError::MissingParent
                | CommentDecodeError::DuplicateContradictoryParent
                | CommentDecodeError::MissingParentKind
                | CommentDecodeError::InvalidParentKind { .. }
                | CommentDecodeError::MalformedParentReference
                | CommentDecodeError::ParentDoesNotMatchRootOrComment => variant.to_string(),
            };
            assert!(!described.is_empty());
        }
    }
}
