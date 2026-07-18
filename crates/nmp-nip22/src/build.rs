//! Immutable NIP-22 kind:1111 draft construction (#572). NEVER signs (the
//! nmp-nip29/nmp-nip68/nmp-blossom discipline): emits an
//! [`nostr::UnsignedEvent`] for the caller's existing signer machinery.
//! Deterministic and byte-identical across Rust/Swift/Kotlin for the same
//! inputs -- `created_at` is caller-supplied (never `Timestamp::now()`
//! internally), exactly like `nmp_nip68::build_picture`.

use nostr::{EventBuilder, Kind, PublicKey, Tag, Timestamp, UnsignedEvent};

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
pub fn compose_top_level_comment(
    root: &CommentRoot,
    author: PublicKey,
    created_at: Timestamp,
    content: String,
) -> UnsignedEvent {
    let mut tags = root_tags(root);
    tags.extend(parent_mirrors_root_tags(root));
    EventBuilder::new(Kind::from(COMMENT_KIND), content)
        .tags(tags)
        .custom_created_at(created_at)
        .build(author)
}

/// Build an unsigned NIP-22 reply: the root tags stay pinned to the
/// thread's root, but the parent becomes the comment event being replied
/// to. Tag order: root tags first, then `["e", parent], ["k", "1111"],
/// ["p", parent_author]?`.
pub fn compose_comment_reply(
    root: &CommentRoot,
    parent: CommentParent,
    author: PublicKey,
    created_at: Timestamp,
    content: String,
) -> UnsignedEvent {
    let mut tags = root_tags(root);
    match parent {
        CommentParent::Root => tags.extend(parent_mirrors_root_tags(root)),
        CommentParent::Comment { event_id, author } => {
            tags.extend(parent_comment_tags(&event_id, author))
        }
    }
    EventBuilder::new(Kind::from(COMMENT_KIND), content)
        .tags(tags)
        .custom_created_at(created_at)
        .build(author)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::Nip73Target;
    use nostr::util::JsonUtil;
    use nostr::{EventId, Keys};

    fn author() -> PublicKey {
        Keys::generate().public_key()
    }

    fn fixed_time() -> Timestamp {
        Timestamp::from(1_700_000_000u64)
    }

    fn tag_rows(event: &UnsignedEvent) -> Vec<Vec<String>> {
        event
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
        let event = compose_top_level_comment(
            &podcast_root(),
            author(),
            fixed_time(),
            "nice episode".to_string(),
        );
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
            author(),
            fixed_time(),
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
            author(),
            fixed_time(),
            "hi".to_string(),
        );
        assert!(!tag_rows(&event).iter().any(|row| row[0] == "p"));
    }

    /// Determinism: identical inputs produce byte-identical unsigned
    /// bodies (the parity/cross-language contract).
    #[test]
    fn compose_is_deterministic() {
        let a = author();
        let mut first =
            compose_top_level_comment(&podcast_root(), a, fixed_time(), "x".to_string());
        let mut second =
            compose_top_level_comment(&podcast_root(), a, fixed_time(), "x".to_string());
        assert_eq!(first.id(), second.id());
        assert_eq!(first.as_json(), second.as_json());
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
        let event = compose_top_level_comment(&root, author(), fixed_time(), "hi".to_string());
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
        let event = compose_top_level_comment(&root, author(), fixed_time(), "hi".to_string());
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
        let event = compose_top_level_comment(&root, author(), fixed_time(), "hi".to_string());
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

/// #572 review finding 4 ("test honesty"): a REAL golden fixture -- a fixed
/// secret key, timestamp, content, and podcast target -- whose composed
/// event id and exact NIP-01 JSON body are pinned as literal constants and
/// asserted identical in Rust (here), Swift (`NIP22Tests.swift`), and
/// Kotlin (`NIP22Test.kt`). Structural identity (all composition happens in
/// Rust behind FFI) is a fair argument for why Swift/Kotlin composing the
/// SAME bytes is likely, but it isn't the demanded proof; this fixture
/// pins the ACTUAL bytes so all three languages assert the same literal,
/// not merely "my own two calls agree with each other".
#[cfg(test)]
pub(crate) mod golden_fixture {
    /// A fixed, arbitrary-but-valid secp256k1 secret key (32 bytes of
    /// `0x01`) -- deterministic across every language/run, never
    /// `Keys::generate()`.
    pub(crate) const SECRET_KEY_HEX: &str =
        "0101010101010101010101010101010101010101010101010101010101010101";
    pub(crate) const AUTHOR_PUBKEY_HEX: &str =
        "1b84c5567b126440995d3ed5aaba0565d71e1834604819ff9c17f5e9d5dd078f";
    pub(crate) const CREATED_AT: u64 = 1_700_000_000;
    pub(crate) const GUID: &str = "golden-guid-572";
    pub(crate) const CONTENT: &str = "golden fixture content";
    pub(crate) const EXPECTED_EVENT_ID_HEX: &str =
        "b1981e70a89150af5ca02548324f3ca2a1fff1b97581d46ab53e11116a553938";
    pub(crate) const EXPECTED_JSON: &str = concat!(
        "{\"id\":\"b1981e70a89150af5ca02548324f3ca2a1fff1b97581d46ab53e11116a553938\",",
        "\"pubkey\":\"1b84c5567b126440995d3ed5aaba0565d71e1834604819ff9c17f5e9d5dd078f\",",
        "\"created_at\":1700000000,\"kind\":1111,",
        "\"tags\":[[\"I\",\"podcast:item:guid:golden-guid-572\"],",
        "[\"K\",\"podcast:item:guid\"],",
        "[\"i\",\"podcast:item:guid:golden-guid-572\"],",
        "[\"k\",\"podcast:item:guid\"]],",
        "\"content\":\"golden fixture content\"}"
    );
}

#[cfg(test)]
mod golden_fixture_tests {
    use super::golden_fixture::*;
    use super::*;
    use crate::target::Nip73Target;
    use nostr::util::JsonUtil;
    use nostr::Keys;

    /// #572 review finding 4: pins the ACTUAL composed bytes (event id +
    /// exact JSON body) for a fixed key/timestamp/content/target -- the
    /// falsifier the issue's decision comment demands, not merely two
    /// in-process Rust calls agreeing with each other
    /// (`compose_is_deterministic`, above, is a DIFFERENT and weaker
    /// falsifier). Swift's and Kotlin's SDK tests assert these SAME
    /// literal constants.
    #[test]
    fn golden_fixture_pins_the_exact_composed_bytes() {
        let keys = Keys::parse(SECRET_KEY_HEX).unwrap();
        let author = keys.public_key();
        assert_eq!(author.to_hex(), AUTHOR_PUBKEY_HEX);
        let root = CommentRoot::External(Nip73Target::podcast_episode_guid(GUID).unwrap());
        let event = compose_top_level_comment(
            &root,
            author,
            Timestamp::from(CREATED_AT),
            CONTENT.to_string(),
        );
        assert_eq!(event.id.unwrap().to_hex(), EXPECTED_EVENT_ID_HEX);
        assert_eq!(event.as_json(), EXPECTED_JSON);
    }
}
