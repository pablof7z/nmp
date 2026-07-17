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
    /// An `I`/`i` value did not carry the prefix its `K`/`k` cell requires
    /// (currently only `K == podcast:item:guid`, which requires the
    /// `podcast:item:guid:` `I`/`i` prefix per NIP-73's own table).
    MalformedExternalValue { got: String },
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
            Self::MalformedExternalValue { got } => {
                write!(
                    f,
                    "I/i value {got:?} does not carry the prefix its K/k cell requires"
                )
            }
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
    /// An addressable root that ALSO pins the event's own id (NIP-22:
    /// "when the parent event is replaceable or addressable, also include
    /// an `e`/`E` tag referencing its id") -- both `A` and `E` present
    /// together, never a contradiction.
    AddressWithEvent {
        coordinate: String,
        event_id: EventId,
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

/// True when `name` appears MORE THAN ONCE among `tags` -- a same-letter
/// duplication (e.g. two `I` tags, or two contradictory `K` tags), distinct
/// from the cross-letter `E`+`I` contradiction the root/parent presence
/// counts catch. `find_tag` only ever sees the first match, so without this
/// check a duplicate of the SAME letter is silently invisible.
fn has_duplicate_tag(tags: &[Vec<String>], name: &str) -> bool {
    tags.iter()
        .filter(|tag| tag.first().map(String::as_str) == Some(name))
        .count()
        > 1
}

fn parse_address_coordinate(coordinate: &str) -> Option<(u16, PublicKey, String)> {
    let mut parts = coordinate.splitn(3, ':');
    let kind = parts.next()?.parse::<u16>().ok()?;
    let pubkey = PublicKey::from_hex(parts.next()?).ok()?;
    let identifier = parts.next()?.to_string();
    Some((kind, pubkey, identifier))
}

/// The root's `A`+`E` co-presence case: an addressable root that ALSO pins
/// the event's own id. Only reached once the caller has established both
/// are present and `I` is absent.
fn decode_root_address_with_event(tags: &[Vec<String>]) -> Result<RawRoot, CommentDecodeError> {
    let a = find_tag(tags, "A").expect("A present: caller already checked");
    let coordinate = a
        .get(1)
        .cloned()
        .ok_or(CommentDecodeError::MalformedRootReference)?;
    if parse_address_coordinate(&coordinate).is_none() {
        return Err(CommentDecodeError::MalformedRootReference);
    }
    let e = find_tag(tags, "E").expect("E present: caller already checked");
    let event_id = e
        .get(1)
        .and_then(|hex| EventId::from_hex(hex).ok())
        .ok_or(CommentDecodeError::MalformedRootReference)?;
    let kind_tag = find_tag(tags, "K")
        .and_then(|k| k.get(1))
        .cloned()
        .ok_or(CommentDecodeError::MissingRootKind)?;
    Ok(RawRoot::AddressWithEvent {
        coordinate,
        event_id,
        kind_tag,
    })
}

fn decode_root(tags: &[Vec<String>]) -> Result<RawRoot, CommentDecodeError> {
    // Same-letter duplicates (two `I` tags, two contradictory `K` tags,
    // etc.) are a distinct malformation from the cross-letter `E`+`I`
    // contradiction below -- `find_tag` only ever sees the first match, so
    // this must be checked explicitly and first.
    for letter in ["E", "A", "I", "K"] {
        if has_duplicate_tag(tags, letter) {
            return Err(CommentDecodeError::DuplicateContradictoryRoot);
        }
    }

    let has_e = find_tag(tags, "E").is_some();
    let has_a = find_tag(tags, "A").is_some();
    let has_i = find_tag(tags, "I").is_some();
    let present = has_e as usize + has_a as usize + has_i as usize;
    if present == 0 {
        return Err(CommentDecodeError::MissingRoot);
    }
    if present > 1 {
        // The ONLY legal multi-tag root combination is `A`+`E` together
        // (NIP-22's "when the parent event is replaceable or addressable,
        // also include an e/E tag referencing its id" allowance, applied
        // symmetrically at root scope). Anything else -- `E`+`I`, `A`+`I`,
        // or all three -- is a genuine contradiction.
        if has_e && has_a && !has_i {
            return decode_root_address_with_event(tags);
        }
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
    /// The parent's `a`+`e` co-presence case -- NIP-22's spec example of a
    /// top-level comment on an addressable root carries BOTH: "when the
    /// parent event is replaceable or addressable, also include an `e` tag
    /// referencing its id."
    AddressWithEvent {
        coordinate: String,
        event_id: EventId,
        kind_tag: String,
    },
    External {
        i_value: String,
        k_value: String,
    },
}

/// The parent's `a`+`e` co-presence case: an addressable parent that ALSO
/// pins the event's own id. Only reached once the caller has established
/// both are present and `i` is absent.
fn decode_parent_address_with_event(tags: &[Vec<String>]) -> Result<RawParent, CommentDecodeError> {
    let a = find_tag(tags, "a").expect("a present: caller already checked");
    let coordinate = a
        .get(1)
        .cloned()
        .ok_or(CommentDecodeError::MalformedParentReference)?;
    if parse_address_coordinate(&coordinate).is_none() {
        return Err(CommentDecodeError::MalformedParentReference);
    }
    let e = find_tag(tags, "e").expect("e present: caller already checked");
    let event_id = e
        .get(1)
        .and_then(|hex| EventId::from_hex(hex).ok())
        .ok_or(CommentDecodeError::MalformedParentReference)?;
    let kind_tag = find_tag(tags, "k")
        .and_then(|k| k.get(1))
        .cloned()
        .ok_or(CommentDecodeError::MissingParentKind)?;
    Ok(RawParent::AddressWithEvent {
        coordinate,
        event_id,
        kind_tag,
    })
}

fn decode_parent(tags: &[Vec<String>]) -> Result<RawParent, CommentDecodeError> {
    // Same-letter duplicates -- see `decode_root`'s identical check.
    for letter in ["e", "a", "i", "k"] {
        if has_duplicate_tag(tags, letter) {
            return Err(CommentDecodeError::DuplicateContradictoryParent);
        }
    }

    let has_e = find_tag(tags, "e").is_some();
    let has_a = find_tag(tags, "a").is_some();
    let has_i = find_tag(tags, "i").is_some();
    let present = has_e as usize + has_a as usize + has_i as usize;
    if present == 0 {
        return Err(CommentDecodeError::MissingParent);
    }
    if present > 1 {
        // The ONLY legal multi-tag parent combination is `a`+`e` together
        // -- NIP-22's own canonical top-level-comment-on-an-addressable-
        // root example. Anything else remains a genuine contradiction.
        if has_e && has_a && !has_i {
            return decode_parent_address_with_event(tags);
        }
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
                event_id: None,
            })
        }
        RawRoot::AddressWithEvent {
            coordinate,
            event_id,
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
                event_id: Some(event_id),
            })
        }
        RawRoot::External { i_value, k_value } => {
            let target = external_target_from_i_k(&i_value, &k_value)?;
            Ok(CommentRoot::External(target))
        }
    }
}

/// Construct the typed [`Nip73Target`] a decoded `I`/`i` + `K`/`k` cell
/// pair names, mapping the target crate's construction error onto the
/// right [`CommentDecodeError`]: a missing podcast-guid prefix is a
/// [`CommentDecodeError::MalformedExternalValue`] (the cell's FORMAT is
/// wrong for what `K`/`k` declares), while an empty cell remains
/// [`CommentDecodeError::EmptyExternalValue`].
fn external_target_from_i_k(
    i_value: &str,
    k_value: &str,
) -> Result<Nip73Target, CommentDecodeError> {
    if k_value == Nip73Target::PODCAST_EPISODE_GUID_KIND {
        return Nip73Target::parse_podcast_episode_guid_i_value(i_value).map_err(|err| match err {
            crate::target::Nip73TargetError::MissingPodcastGuidPrefix => {
                CommentDecodeError::MalformedExternalValue {
                    got: i_value.to_string(),
                }
            }
            crate::target::Nip73TargetError::EmptyValue
            | crate::target::Nip73TargetError::EmptyKind => CommentDecodeError::EmptyExternalValue,
        });
    }
    Nip73Target::general(i_value, k_value).map_err(|_| CommentDecodeError::EmptyExternalValue)
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
            if pe == event_id {
                // Same id referenced: this can ONLY be the root-mirror
                // case, so its kind MUST equal the root's own kind. A
                // single event cannot simultaneously BE the root (kind
                // `*kind`) and be a `k=1111` comment being replied to --
                // that is a case-pair contradiction, not a coincidental
                // reply, even when `parent_kind == COMMENT_KIND`.
                return if parent_kind == *kind {
                    Ok(CommentParent::Root)
                } else {
                    Err(CommentDecodeError::ParentDoesNotMatchRootOrComment)
                };
            }
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
                ..
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
        (
            CommentRoot::Address {
                author: root_author,
                kind,
                identifier,
                event_id: root_event_id,
            },
            RawParent::AddressWithEvent {
                coordinate,
                event_id,
                kind_tag,
            },
        ) => {
            // NIP-22's own canonical top-level comment on an addressable
            // root: the parent carries BOTH `a` (matching the root
            // coordinate) and `e` (the addressable event's own id). Cross-
            // check the `e` against the root's pinned id when the root
            // has one; when it doesn't, accept the parent's `e` as
            // unverified auxiliary info (the root simply didn't record it).
            let parent_kind =
                kind_tag
                    .parse::<u16>()
                    .map_err(|_| CommentDecodeError::InvalidParentKind {
                        got: kind_tag.clone(),
                    })?;
            let expected = CommentRoot::address_coordinate(*kind, root_author, identifier);
            if *coordinate != expected || parent_kind != *kind {
                return Err(CommentDecodeError::ParentDoesNotMatchRootOrComment);
            }
            if let Some(root_event_id) = root_event_id {
                if event_id != root_event_id {
                    return Err(CommentDecodeError::ParentDoesNotMatchRootOrComment);
                }
            }
            Ok(CommentParent::Root)
        }
        (CommentRoot::External(target), RawParent::External { i_value, k_value }) => {
            if *i_value == target.i_value() && k_value == target.k_value() {
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
            row(&["I", "podcast:item:guid:guid-1"]),
            row(&["K", "podcast:item:guid"]),
            row(&["i", "podcast:item:guid:guid-DIFFERENT"]),
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
            row(&["I", "podcast:item:guid:guid-1"]),
            row(&["K", "podcast:item:guid"]),
            row(&["i", "podcast:item:guid:guid-1"]),
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

    /// #572 review finding 1: a `K == podcast:item:guid` cell whose `I`
    /// value is the BARE guid (no `podcast:item:guid:` prefix) is a typed
    /// refusal, never silently accepted as-is -- a bare-guid comment would
    /// split the episode's thread from conformant clients.
    #[test]
    fn podcast_guid_missing_prefix_is_rejected() {
        let tags = vec![
            row(&["I", "guid-1"]),
            row(&["K", "podcast:item:guid"]),
            row(&["i", "guid-1"]),
            row(&["k", "podcast:item:guid"]),
        ];
        let err = decode_comment(
            EventId::from_slice(&[11; 32]).unwrap(),
            keys().public_key(),
            1000,
            COMMENT_KIND,
            &tags,
            "",
        )
        .unwrap_err();
        assert_eq!(
            err,
            CommentDecodeError::MalformedExternalValue {
                got: "guid-1".to_string()
            }
        );
    }

    /// Duplicate contradictory root tags (both E and I present) are
    /// rejected.
    #[test]
    fn duplicate_contradictory_root_tags_are_rejected() {
        let tags = vec![
            row(&["E", &EventId::from_slice(&[6; 32]).unwrap().to_hex()]),
            row(&["I", "podcast:item:guid:guid-1"]),
            row(&["K", "podcast:item:guid"]),
            row(&["i", "podcast:item:guid:guid-1"]),
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

    /// #572 review finding 3: same-letter duplicates (two DIFFERENT `I`
    /// tags) are a typed rejection, not a silent "first one wins".
    #[test]
    fn duplicate_same_letter_root_tags_are_rejected() {
        let tags = vec![
            row(&["I", "podcast:item:guid:guid-1"]),
            row(&["I", "podcast:item:guid:guid-2"]),
            row(&["K", "podcast:item:guid"]),
            row(&["i", "podcast:item:guid:guid-1"]),
            row(&["k", "podcast:item:guid"]),
        ];
        let err = decode_comment(
            EventId::from_slice(&[12; 32]).unwrap(),
            keys().public_key(),
            1000,
            COMMENT_KIND,
            &tags,
            "",
        )
        .unwrap_err();
        assert_eq!(err, CommentDecodeError::DuplicateContradictoryRoot);
    }

    /// #572 review finding 3: two contradictory `K` root tags are rejected
    /// the same way.
    #[test]
    fn duplicate_contradictory_k_root_tags_are_rejected() {
        let tags = vec![
            row(&["I", "podcast:item:guid:guid-1"]),
            row(&["K", "podcast:item:guid"]),
            row(&["K", "some-other-namespace"]),
            row(&["i", "podcast:item:guid:guid-1"]),
            row(&["k", "podcast:item:guid"]),
        ];
        let err = decode_comment(
            EventId::from_slice(&[13; 32]).unwrap(),
            keys().public_key(),
            1000,
            COMMENT_KIND,
            &tags,
            "",
        )
        .unwrap_err();
        assert_eq!(err, CommentDecodeError::DuplicateContradictoryRoot);
    }

    /// #572 review finding 3: same-letter duplicate PARENT tags (two `i`
    /// tags) are rejected the same way as root duplicates.
    #[test]
    fn duplicate_same_letter_parent_tags_are_rejected() {
        let tags = vec![
            row(&["I", "podcast:item:guid:guid-1"]),
            row(&["K", "podcast:item:guid"]),
            row(&["i", "podcast:item:guid:guid-1"]),
            row(&["i", "podcast:item:guid:guid-2"]),
            row(&["k", "podcast:item:guid"]),
        ];
        let err = decode_comment(
            EventId::from_slice(&[14; 32]).unwrap(),
            keys().public_key(),
            1000,
            COMMENT_KIND,
            &tags,
            "",
        )
        .unwrap_err();
        assert_eq!(err, CommentDecodeError::DuplicateContradictoryParent);
    }

    /// #572 review finding 2: NIP-22's own canonical top-level comment on
    /// an addressable root -- the spec example carries BOTH `a`/`A` AND
    /// `e`/`E` in the parent/root scope ("when the parent event is
    /// replaceable or addressable, also include an `e` tag referencing its
    /// id"). This must decode, not be rejected as
    /// `DuplicateContradictoryRoot`/`Parent`.
    #[test]
    fn spec_canonical_address_root_with_accompanying_event_id_decodes() {
        let root_author = keys().public_key();
        let pinned_id = EventId::from_slice(&[15; 32]).unwrap();
        let coordinate = format!("30023:{}:my-article", root_author.to_hex());
        let tags = vec![
            row(&["A", &coordinate]),
            row(&["K", "30023"]),
            row(&["P", &root_author.to_hex()]),
            row(&["E", &pinned_id.to_hex()]),
            row(&["a", &coordinate]),
            row(&["k", "30023"]),
            row(&["p", &root_author.to_hex()]),
            row(&["e", &pinned_id.to_hex()]),
        ];
        let decoded = decode_comment(
            EventId::from_slice(&[16; 32]).unwrap(),
            keys().public_key(),
            1000,
            COMMENT_KIND,
            &tags,
            "nice post",
        )
        .expect("NIP-22's own canonical addressable-root shape must decode");
        assert_eq!(
            decoded.root,
            CommentRoot::Address {
                author: root_author,
                kind: 30023,
                identifier: "my-article".to_string(),
                event_id: Some(pinned_id),
            }
        );
        assert_eq!(decoded.parent, CommentParent::Root);
    }

    /// The accompanying `e`/`E` is a SHOULD, not a MUST: an addressable
    /// root/parent WITHOUT it remains fully legal (already covered by
    /// `top_level_comment_on_address_root_mirrors_the_coordinate` in
    /// `build.rs`'s own tests) -- this is the a-only decode counterpart,
    /// proven directly against raw tags.
    #[test]
    fn address_root_without_accompanying_event_id_still_decodes() {
        let root_author = keys().public_key();
        let coordinate = format!("30023:{}:my-article", root_author.to_hex());
        let tags = vec![
            row(&["A", &coordinate]),
            row(&["K", "30023"]),
            row(&["a", &coordinate]),
            row(&["k", "30023"]),
        ];
        let decoded = decode_comment(
            EventId::from_slice(&[17; 32]).unwrap(),
            keys().public_key(),
            1000,
            COMMENT_KIND,
            &tags,
            "hi",
        )
        .expect("a-only address root must decode");
        assert_eq!(
            decoded.root,
            CommentRoot::Address {
                author: root_author,
                kind: 30023,
                identifier: "my-article".to_string(),
                event_id: None,
            }
        );
        assert_eq!(decoded.parent, CommentParent::Root);
    }

    /// #572 review finding 3: an Event root's parent `e` pointing at the
    /// SAME id as the root, but whose `k` is `1111` while the root's `K`
    /// says a different kind, is a case-pair DISAGREEMENT -- never silently
    /// accepted as "a reply to the root treated as a comment".
    #[test]
    fn same_id_parent_with_mismatched_kind_is_a_case_pair_disagreement() {
        let root_id = EventId::from_slice(&[18; 32]).unwrap();
        let tags = vec![
            row(&["E", &root_id.to_hex()]),
            row(&["K", "1"]),
            row(&["e", &root_id.to_hex()]),
            row(&["k", "1111"]),
        ];
        let err = decode_comment(
            EventId::from_slice(&[19; 32]).unwrap(),
            keys().public_key(),
            1000,
            COMMENT_KIND,
            &tags,
            "",
        )
        .unwrap_err();
        assert_eq!(err, CommentDecodeError::ParentDoesNotMatchRootOrComment);
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
            CommentDecodeError::MalformedExternalValue { got: String::new() },
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
                | CommentDecodeError::MalformedExternalValue { .. }
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
