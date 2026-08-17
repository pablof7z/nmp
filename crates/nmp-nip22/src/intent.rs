//! Compose a NIP-22 comment and its publishable `WriteIntent` (#572, folded
//! onto the one tagging door in #1243).
//!
//! The tag SHAPE is no longer here. `nmp-grammar` owns kind:1111 and the
//! uppercase-root/lowercase-parent row shape, because that shape is what
//! `EventBuilder::tag` produces for every non-text-note target and having a
//! second copy of it was the whole defect: the hand-built composer emitted
//! `["E", <id>]` -- two cells -- where NIP-22 defines
//! `["E", <id>, <relay>, <pubkey>]`, six times in one file.
//!
//! What stays here is NIP-22's own verb. [`comment_intent`] always composes a
//! kind:1111 comment, including on a text note, where the general
//! `nmp_grammar::reply_to` would compose a NIP-10 reply instead. An app that
//! wants "the ordinary reply for whatever this is" calls that; an app that
//! wants "a NIP-22 comment on this specifically" calls this.

use nmp_grammar::{
    EventBuilder, Identity, Modifiers, RootScope, WriteIntent, WritePayload,
    WriteRouting, COMMENT_KIND,
};
use nostr::Kind;

/// Compose an unsigned kind:1111 comment on `target`.
///
/// Two calls through the one door: the uppercase root scope naming the
/// thread's root, then the lowercase rows naming the target itself. Neither
/// call states a relationship -- the root is read from the target's own rows
/// (`nmp_grammar::ThreadPosition`), so commenting on a root and commenting on
/// a deep reply are the same call and cannot be got backwards.
pub fn compose_comment(target: &impl RootScope, content: String) -> EventBuilder {
    EventBuilder::new(Kind::from(COMMENT_KIND))
        .tag(target.uppercase())
        .tag(target)
        .content(content)
}

/// Compose a `WriteIntent` for a NIP-22 comment on `target`. This crate adds
/// no routing or identity policy beyond the ordinary defaults every write has.
pub fn comment_intent(target: &impl RootScope, content: String) -> WriteIntent {
    WriteIntent {
        payload: WritePayload::Event(compose_comment(target, content)),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::root::CommentRoot;
    use nmp_nip73::Nip73;
    use nostr::{EventBuilder as NostrBuilder, EventId, Keys, PublicKey, Tag, Timestamp};

    fn podcast_root() -> CommentRoot {
        CommentRoot::External(Nip73::podcast_episode("guid-1").unwrap())
    }

    fn rows(builder: &EventBuilder) -> Vec<Vec<String>> {
        builder
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect()
    }

    fn comment(root_rows: Vec<Tag>) -> nostr::Event {
        NostrBuilder::new(Kind::from(COMMENT_KIND), "parent comment")
            .tags(root_rows)
            .custom_created_at(Timestamp::from(1_700_000_000))
            .sign_with_keys(&Keys::generate())
            .expect("test event signs")
    }

    #[test]
    fn comment_intent_is_a_builder_on_the_auto_route() {
        let intent = comment_intent(&podcast_root(), "hi".to_string());
        assert!(matches!(
            &intent.payload,
            WritePayload::Event(builder) if builder.created_at.is_none()
        ));
        assert!(matches!(intent.routing, WriteRouting::Auto));
        assert_eq!(intent.identity, Identity::Active);
    }

    /// #572's exact required tag shape survives the fold: a top-level comment
    /// on an external content id mirrors it, uppercase then lowercase.
    #[test]
    fn a_top_level_external_comment_mirrors_the_id_uppercase_then_lowercase() {
        let built = compose_comment(&podcast_root(), "nice episode".to_string());
        assert_eq!(built.kind, Kind::from(COMMENT_KIND));
        assert_eq!(
            rows(&built),
            vec![
                vec!["I".to_string(), "podcast:item:guid:guid-1".to_string()],
                vec!["K".to_string(), "podcast:item:guid".to_string()],
                vec!["i".to_string(), "podcast:item:guid:guid-1".to_string()],
                vec!["k".to_string(), "podcast:item:guid".to_string()],
            ]
        );
    }

    /// The replacement for the deleted two-argument reply composer, and a
    /// strict improvement on it: replying to a comment EVENT reads that
    /// event's own uppercase root scope off the wire, so the root cannot be
    /// restated wrongly by a caller who thought it knew.
    #[test]
    fn replying_to_a_comment_keeps_the_root_the_wire_states() {
        let root_id = EventId::from_slice(&[3; 32]).unwrap();
        let root_author = Keys::generate().public_key();
        let parent = comment(vec![
            Tag::parse(["E", &root_id.to_hex()]).unwrap(),
            Tag::parse(["K", "30023"]).unwrap(),
            Tag::parse(["P", &root_author.to_hex()]).unwrap(),
        ]);

        let built = compose_comment(&parent, "agreed".to_string());
        let emitted = rows(&built);
        assert_eq!(
            emitted.iter().find(|row| row[0] == "E").map(|row| &row[1]),
            Some(&root_id.to_hex()),
            "the root stays what the parent's own rows say it is"
        );
        assert_eq!(
            emitted.iter().find(|row| row[0] == "K").map(|row| &row[1]),
            Some(&"30023".to_string())
        );
        assert_eq!(
            emitted.iter().find(|row| row[0] == "e").map(|row| &row[1]),
            Some(&parent.id.to_hex()),
            "the parent is the comment event being replied to"
        );
        assert_eq!(
            emitted.iter().find(|row| row[0] == "k").map(|row| &row[1]),
            Some(&"1111".to_string())
        );
        assert!(emitted
            .iter()
            .any(|row| row[0] == "p" && row[1] == parent.pubkey.to_hex()));
        for row in &emitted {
            assert!(
                !row.contains(&"root".to_string()) && !row.contains(&"reply".to_string()),
                "NIP-22 states importance with case, never a marker: {row:?}"
            );
        }
    }

    /// A parent author NMP does not know produces no `p` row -- never a
    /// placeholder.
    #[test]
    fn an_unknown_author_omits_the_p_row_rather_than_inventing_one() {
        let root = CommentRoot::Event {
            event_id: EventId::from_slice(&[9; 32]).unwrap(),
            kind: 30023,
            author: None,
        };
        let built = compose_comment(&root, "hi".to_string());
        assert!(!rows(&built)
            .iter()
            .any(|row| row[0] == "p" || row[0] == "P"));
    }

    /// The schema is a pure function of its inputs. Note what is NOT
    /// asserted: byte identity of the resulting events. Two composes of the
    /// same comment differ in the time NMP stamped them, and differing is what
    /// timestamps are for.
    #[test]
    fn compose_is_a_pure_function_of_its_inputs() {
        let first = compose_comment(&podcast_root(), "x".to_string());
        let second = compose_comment(&podcast_root(), "x".to_string());
        assert_eq!(first, second);
        assert_eq!(first.created_at, None);
        let _: PublicKey = Keys::generate().public_key();
    }
}
