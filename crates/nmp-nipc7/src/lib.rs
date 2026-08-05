//! `nmp-nipc7` -- the pure NIP-C7 kind:9 chat schema owner (#838).
//!
//! C7 owns the event kind and its reply row. It does not own NIP-29 group
//! context, NIP-27 inline mention materialization, or client notification
//! policy, and it owns no write policy either -- so its composers own SCHEMA
//! ONLY and return an [`EventBuilder`], leaving routing and identity to
//! whoever does. They name no author and read no clock: the engine resolves
//! the identity and stamps the time at acceptance.
//!
//! ## Why C7 offers its own reply verb
//!
//! Kind:9 must NOT become a NIP-22 comment. NIP-29 clients MUST only fetch
//! kind 9, so a 1111 reply inside a group would be invisible to every one of
//! them. That is why a schema with its own reply convention offers **its own
//! verb** ([`chat_reply`]) rather than an arm in a general dispatcher
//! (#1243): the app is already holding this crate, and the general
//! `nmp_grammar::reply_to` stays a two-way split with nothing to grow.
//!
//! The rows themselves still come from the one tagging door, so a chat reply
//! carries the relay hint, the author slot and the companion `p` row that
//! every other pointer in NMP carries.

use nmp_grammar::{EventBuilder, RootScope};
use nostr::Kind;

pub const CHAT_KIND: u16 = 9;

/// Compose a kind:9 chat with no policy-added tags.
pub fn compose_chat(content: String) -> EventBuilder {
    EventBuilder::new(Kind::from(CHAT_KIND)).content(content)
}

/// Compose a kind:9 reply to `target`.
///
/// The reply row is `e`, not `q`. The composer this replaces emitted
/// `["q", <id>, <relay>, <author>]` (#1243), and a `q` row is NIP-18's QUOTE
/// marker whose entire stated purpose is that *"quote reposts are not pulled
/// and included as replies in threads"* -- so a C7 client reading it correctly
/// would place the message outside the thread it is replying to. Worse, that
/// composer emitted the `q` with nothing in the content quoting it, so the
/// half-formed quote could not render either. `nmp_grammar::text!` is how a
/// chat message actually quotes something now, and it makes the row and the
/// inline reference come from one statement so they cannot diverge again.
pub fn chat_reply(target: &impl RootScope) -> EventBuilder {
    EventBuilder::new(Kind::from(CHAT_KIND)).tag(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nmp_grammar::{text, Modifiers};
    use nostr::{EventBuilder as NostrBuilder, Keys, PublicKey, RelayUrl, Tag, Timestamp};

    fn author() -> PublicKey {
        Keys::generate().public_key()
    }

    fn chat(tags: Vec<Tag>) -> nostr::Event {
        NostrBuilder::new(Kind::from(CHAT_KIND), "parent")
            .tags(tags)
            .custom_created_at(Timestamp::from(1_700_000_000))
            .sign_with_keys(&Keys::generate())
            .expect("test event signs")
    }

    fn rows(event: &EventBuilder) -> Vec<Vec<String>> {
        event
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect()
    }

    #[test]
    fn chat_is_kind_9_without_group_or_notification_policy() {
        let event = compose_chat("hello".to_string());
        assert_eq!(event.kind, Kind::from(CHAT_KIND));
        assert_eq!(event.content, "hello");
        assert!(event.tags.is_empty());
        assert_eq!(
            event.created_at, None,
            "a schema-only composer invents no timestamp"
        );
    }

    /// #1243's origin defect, inverted into a falsifier: a reply POINTS with
    /// `e` and never with `q`. A `q` row is NIP-18's quote marker, whose
    /// stated purpose is keeping the referenced event OUT of the thread --
    /// exactly the opposite of a reply. The composer also stays free of NIP-29
    /// group context (`h`) and of `previous`, which are not C7's to add.
    #[test]
    fn chat_reply_points_with_e_and_never_q_or_h_or_previous_rows() {
        let parent = chat(vec![]);
        let event = chat_reply(&parent);

        assert_eq!(event.kind, Kind::from(CHAT_KIND));
        let emitted = rows(&event);
        assert!(
            emitted
                .iter()
                .any(|row| row[0] == "e" && row[1] == parent.id.to_hex()),
            "a reply points with e: {emitted:?}"
        );
        for row in &emitted {
            assert_ne!(row[0], "q", "a reply is not a quote");
            assert_ne!(row[0], "h", "group context is NIP-29's, never C7's");
            assert_ne!(row[0], "previous", "C7 mints no group timeline evidence");
        }
        // Kind:9 replies stay kind:9. Turning one into a NIP-22 comment would
        // make it invisible: NIP-29 clients MUST only fetch kind 9.
        assert_eq!(event.kind, Kind::from(CHAT_KIND));
    }

    /// The reply row carries what the one door fills everywhere: the relay
    /// hint, the author slot and the companion `p` row. Before #1243 exactly
    /// one row in the whole tree filled the hint, and it was this one.
    #[test]
    fn a_chat_reply_carries_the_hint_the_author_slot_and_the_p_row() {
        let relay = RelayUrl::parse("wss://chat.example.com").unwrap();
        let parent = chat(vec![]);
        let event = chat_reply(&parent.from_relay(relay.clone()));

        let emitted = rows(&event);
        let e_row = emitted.iter().find(|row| row[0] == "e").expect("an e row");
        assert_eq!(e_row[2], relay.to_string(), "the hint slot is filled");
        assert_eq!(
            e_row[3],
            parent.pubkey.to_hex(),
            "the author slot is filled"
        );
        assert!(emitted
            .iter()
            .any(|row| row[0] == "p" && row[1] == parent.pubkey.to_hex()));
    }

    /// A quote in a chat message is content interpolation, so the `q` row and
    /// the rendered `nostr:nevent1…` come from one statement and cannot
    /// disagree -- which is precisely what the deleted quote-shaped reply
    /// composer got wrong.
    #[test]
    fn a_quote_and_its_row_still_come_from_one_statement() {
        let quoted = chat(vec![]);
        let _ = author();
        let event = compose_chat(String::new()).content(text!("look: {}", &quoted));
        assert!(event.content.contains("nostr:nevent1"));
        let emitted = rows(&event);
        assert_eq!(emitted[0][0], "q");
        assert_eq!(emitted[0][1], quoted.id.to_hex());
    }
}
