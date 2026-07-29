//! Immutable NIP-22 kind:1111 composition (#572). NEVER signs (the
//! nmp-nip29/nmp-nip68/nmp-blossom discipline) and owns the SCHEMA only, so
//! it returns an [`EventBuilder`] and leaves durability, routing and
//! identity to whoever owns the write policy. It reads no clock and queries
//! no account -- the whole crate keeps its zero engine dependency -- but the
//! author and the timestamp are no longer caller parameters either: the
//! engine stamps both at acceptance, and the byte-reproducibility argument
//! that once justified taking them died with it (nothing needed it, and if
//! anything had, it could never have been one NIP's concern).

use nmp_grammar::EventBuilder;
use nostr::{Kind, PublicKey, Tag};

use crate::root::{CommentParent, CommentRoot, COMMENT_KIND};

/// One root's uppercase tag triple: `[root-tag, K, P?]`, in that order.
fn root_tags(root: &CommentRoot) -> Vec<Tag> {
    let mut tags = Vec::with_capacity(3);
    match root {
        CommentRoot::Event {
            event_id,
            kind,
            author,
        } => {
            tags.push(Tag::parse(["E", &event_id.to_hex()]).expect("non-empty E row"));
            tags.push(Tag::parse(["K", &kind.to_string()]).expect("non-empty K row"));
            if let Some(author) = author {
                tags.push(Tag::parse(["P", &author.to_hex()]).expect("non-empty P row"));
            }
        }
        CommentRoot::Address {
            author,
            kind,
            identifier,
            event_id,
        } => {
            let coordinate = CommentRoot::address_coordinate(*kind, author, identifier);
            tags.push(Tag::parse(["A", &coordinate]).expect("non-empty A row"));
            tags.push(Tag::parse(["K", &kind.to_string()]).expect("non-empty K row"));
            tags.push(Tag::parse(["P", &author.to_hex()]).expect("non-empty P row"));
            if let Some(event_id) = event_id {
                tags.push(Tag::parse(["E", &event_id.to_hex()]).expect("non-empty E row"));
            }
        }
        CommentRoot::External(target) => {
            tags.push(Tag::parse(["I", &target.i_value()]).expect("non-empty I row"));
            tags.push(Tag::parse(["K", target.k_value()]).expect("non-empty K row"));
        }
    }
    tags
}

/// The lowercase mirror of [`root_tags`] -- a TOP-LEVEL comment's parent
/// tag triple: `[parent-tag, k, p?]`, identical identity to the root, just
/// lowercased.
fn parent_mirrors_root_tags(root: &CommentRoot) -> Vec<Tag> {
    let mut tags = Vec::with_capacity(3);
    match root {
        CommentRoot::Event {
            event_id,
            kind,
            author,
        } => {
            tags.push(Tag::parse(["e", &event_id.to_hex()]).expect("non-empty e row"));
            tags.push(Tag::parse(["k", &kind.to_string()]).expect("non-empty k row"));
            if let Some(author) = author {
                tags.push(Tag::parse(["p", &author.to_hex()]).expect("non-empty p row"));
            }
        }
        CommentRoot::Address {
            author,
            kind,
            identifier,
            event_id,
        } => {
            let coordinate = CommentRoot::address_coordinate(*kind, author, identifier);
            tags.push(Tag::parse(["a", &coordinate]).expect("non-empty a row"));
            tags.push(Tag::parse(["k", &kind.to_string()]).expect("non-empty k row"));
            tags.push(Tag::parse(["p", &author.to_hex()]).expect("non-empty p row"));
            if let Some(event_id) = event_id {
                // NIP-22: "when the parent event is replaceable or
                // addressable, also include an `e` tag referencing its id"
                // -- the coordinate alone doesn't pin a specific revision.
                tags.push(Tag::parse(["e", &event_id.to_hex()]).expect("non-empty e row"));
            }
        }
        CommentRoot::External(target) => {
            tags.push(Tag::parse(["i", &target.i_value()]).expect("non-empty i row"));
            tags.push(Tag::parse(["k", target.k_value()]).expect("non-empty k row"));
        }
    }
    tags
}

/// A reply's parent tag pair/triple: `["e", parent_event_id], ["k",
/// "1111"], ["p", parent_author]?`.
fn parent_comment_tags(event_id: &nostr::EventId, author: Option<PublicKey>) -> Vec<Tag> {
    let mut tags = Vec::with_capacity(3);
    tags.push(Tag::parse(["e", &event_id.to_hex()]).expect("non-empty e row"));
    tags.push(Tag::parse(["k", &COMMENT_KIND.to_string()]).expect("non-empty k row"));
    if let Some(author) = author {
        tags.push(Tag::parse(["p", &author.to_hex()]).expect("non-empty p row"));
    }
    tags
}

/// Build an unsigned top-level NIP-22 comment on `root`: the parent tags
/// mirror the root tags exactly (lowercased). Tag order: root tags first
/// (`E`/`A`/`I`, `K`, `P`?), then the mirrored parent tags (`e`/`a`/`i`,
/// `k`, `p`?).
pub fn compose_top_level_comment(root: &CommentRoot, content: String) -> EventBuilder {
    let mut tags = root_tags(root);
    tags.extend(parent_mirrors_root_tags(root));
    EventBuilder {
        kind: Kind::from(COMMENT_KIND),
        tags,
        content,
        created_at: None,
    }
}

/// Build an unsigned NIP-22 reply: the root tags stay pinned to the
/// thread's root, but the parent becomes the comment event being replied
/// to. Tag order: root tags first, then `["e", parent], ["k", "1111"],
/// ["p", parent_author]?`.
pub fn compose_comment_reply(
    root: &CommentRoot,
    parent: CommentParent,
    content: String,
) -> EventBuilder {
    let mut tags = root_tags(root);
    match parent {
        CommentParent::Root => tags.extend(parent_mirrors_root_tags(root)),
        CommentParent::Comment { event_id, author } => {
            tags.extend(parent_comment_tags(&event_id, author))
        }
    }
    EventBuilder {
        kind: Kind::from(COMMENT_KIND),
        tags,
        content,
        created_at: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::Nip73Target;
    use nostr::{EventId, Keys};

    fn author() -> PublicKey {
        Keys::generate().public_key()
    }

    fn tag_rows(builder: &EventBuilder) -> Vec<Vec<String>> {
        builder
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect()
    }

    fn podcast_root() -> CommentRoot {
        CommentRoot::External(Nip73Target::podcast_episode_guid("guid-123").unwrap())
    }

    /// #572's exact required tag shape: top-level parent equal to the root
    /// (`i` + `k=podcast:item:guid`).
    #[test]
    fn top_level_podcast_comment_has_exact_required_tags() {
        let event = compose_top_level_comment(&podcast_root(), "nice episode".to_string());
        assert_eq!(event.kind, Kind::from(COMMENT_KIND));
        assert_eq!(
            tag_rows(&event),
            vec![
                vec!["I".to_string(), "podcast:item:guid:guid-123".to_string()],
                vec!["K".to_string(), "podcast:item:guid".to_string()],
                vec!["i".to_string(), "podcast:item:guid:guid-123".to_string()],
                vec!["k".to_string(), "podcast:item:guid".to_string()],
            ]
        );
    }

    /// #572's exact required tag shape: a reply's root stays the podcast
    /// target while its parent is a comment event (`e` + `k=1111` + parent
    /// `p` when known).
    #[test]
    fn reply_keeps_podcast_root_and_points_parent_at_the_comment_event() {
        let parent_author = author();
        let parent_id = EventId::from_slice(&[7; 32]).unwrap();
        let event = compose_comment_reply(
            &podcast_root(),
            CommentParent::Comment {
                event_id: parent_id,
                author: Some(parent_author),
            },
            "agreed".to_string(),
        );
        assert_eq!(
            tag_rows(&event),
            vec![
                vec!["I".to_string(), "podcast:item:guid:guid-123".to_string()],
                vec!["K".to_string(), "podcast:item:guid".to_string()],
                vec!["e".to_string(), parent_id.to_hex()],
                vec!["k".to_string(), "1111".to_string()],
                vec!["p".to_string(), parent_author.to_hex()],
            ]
        );
    }

    /// A reply with an unknown parent author omits the `p` tag entirely --
    /// never a placeholder.
    #[test]
    fn reply_with_unknown_parent_author_omits_p_tag() {
        let parent_id = EventId::from_slice(&[9; 32]).unwrap();
        let event = compose_comment_reply(
            &podcast_root(),
            CommentParent::Comment {
                event_id: parent_id,
                author: None,
            },
            "hi".to_string(),
        );
        assert!(!tag_rows(&event).iter().any(|row| row[0] == "p"));
    }

    /// The schema is a pure function of its inputs -- same root, same
    /// content, same tags in the same order. Note what is NOT asserted:
    /// byte identity of the resulting events. Two composes of the same
    /// comment differ in the time NMP stamped them, and differing is what
    /// timestamps are for; a reproducible-bytes rule could never have been
    /// one NIP's concern anyway.
    #[test]
    fn compose_is_a_pure_function_of_its_inputs() {
        let first = compose_top_level_comment(&podcast_root(), "x".to_string());
        let second = compose_top_level_comment(&podcast_root(), "x".to_string());
        assert_eq!(first, second);
        assert_eq!(first.created_at, None);
    }

    /// A top-level comment on an Event root mirrors it exactly, including
    /// the optional root author.
    #[test]
    fn top_level_comment_on_event_root_mirrors_exactly() {
        let root_author = author();
        let root_id = EventId::from_slice(&[3; 32]).unwrap();
        let root = CommentRoot::Event {
            event_id: root_id,
            kind: 1,
            author: Some(root_author),
        };
        let event = compose_top_level_comment(&root, "hi".to_string());
        assert_eq!(
            tag_rows(&event),
            vec![
                vec!["E".to_string(), root_id.to_hex()],
                vec!["K".to_string(), "1".to_string()],
                vec!["P".to_string(), root_author.to_hex()],
                vec!["e".to_string(), root_id.to_hex()],
                vec!["k".to_string(), "1".to_string()],
                vec!["p".to_string(), root_author.to_hex()],
            ]
        );
    }

    /// A top-level comment on an Address root with no pinned event id
    /// mirrors the coordinate alone -- no `E`/`e` tag when there is nothing
    /// to pin.
    #[test]
    fn top_level_comment_on_address_root_mirrors_the_coordinate() {
        let root_author = author();
        let root = CommentRoot::Address {
            author: root_author,
            kind: 30023,
            identifier: "my-article".to_string(),
            event_id: None,
        };
        let event = compose_top_level_comment(&root, "hi".to_string());
        let coordinate = format!("30023:{}:my-article", root_author.to_hex());
        assert_eq!(
            tag_rows(&event),
            vec![
                vec!["A".to_string(), coordinate.clone()],
                vec!["K".to_string(), "30023".to_string()],
                vec!["P".to_string(), root_author.to_hex()],
                vec!["a".to_string(), coordinate],
                vec!["k".to_string(), "30023".to_string()],
                vec!["p".to_string(), root_author.to_hex()],
            ]
        );
    }

    /// #572 review finding 2: an Address root that DOES pin an event id
    /// gets the accompanying `E`/`e` NIP-22 instructs writers to include
    /// ("when the parent event is replaceable or addressable, also include
    /// an `e` tag referencing its id") at both root and parent-mirror
    /// scope.
    #[test]
    fn top_level_comment_on_address_root_with_event_id_also_emits_e() {
        let root_author = author();
        let pinned_id = EventId::from_slice(&[5; 32]).unwrap();
        let root = CommentRoot::Address {
            author: root_author,
            kind: 30023,
            identifier: "my-article".to_string(),
            event_id: Some(pinned_id),
        };
        let event = compose_top_level_comment(&root, "hi".to_string());
        let coordinate = format!("30023:{}:my-article", root_author.to_hex());
        assert_eq!(
            tag_rows(&event),
            vec![
                vec!["A".to_string(), coordinate.clone()],
                vec!["K".to_string(), "30023".to_string()],
                vec!["P".to_string(), root_author.to_hex()],
                vec!["E".to_string(), pinned_id.to_hex()],
                vec!["a".to_string(), coordinate],
                vec!["k".to_string(), "30023".to_string()],
                vec!["p".to_string(), root_author.to_hex()],
                vec!["e".to_string(), pinned_id.to_hex()],
            ]
        );
    }
}

// #572 review finding 4 once pinned a "golden fixture" here: a fixed key,
// timestamp and content whose composed event id and exact NIP-01 JSON were
// asserted identical in Rust, Swift and Kotlin. It is gone, deliberately.
// The composer no longer produces bytes at all -- it produces a schema, and
// the author and the timestamp that completed those bytes are decided at
// acceptance. Reproducible bytes were rejected as a requirement outright:
// if they were genuinely needed they could not be one NIP's concern, they
// would be every event's, and they are enforced nowhere and wanted
// nowhere. What all three languages still assert is the thing this crate
// actually owns -- the exact tag rows, in the exact order, for each root
// and parent shape.
