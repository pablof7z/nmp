//! `nmp-nipc7` -- the pure NIP-C7 kind:9 chat schema owner (#838).
//!
//! C7 owns the event kind and its NIP-18 `q` reply row. It does not own
//! NIP-29 group context, NIP-27 inline mention materialization, or client
//! notification policy, and it owns no write policy either -- so its
//! composers own SCHEMA ONLY and return an [`EventBuilder`], leaving
//! durability, routing and identity to whoever does. They name no author
//! and read no clock: the engine resolves the identity and stamps the time
//! at acceptance.

use nmp_grammar::EventBuilder;
use nostr::{EventId, Kind, PublicKey, RelayUrl, Tag};

pub const CHAT_KIND: u16 = 9;

/// The complete event reference required for a C7/NIP-18 `q` reply row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatReply {
    event_id: EventId,
    relay: RelayUrl,
    author: PublicKey,
}

impl ChatReply {
    pub fn new(event_id: EventId, relay: RelayUrl, author: PublicKey) -> Self {
        Self {
            event_id,
            relay,
            author,
        }
    }
}

/// Compose a kind:9 chat with no policy-added tags.
pub fn compose_chat(content: String) -> EventBuilder {
    EventBuilder::new(Kind::from(CHAT_KIND)).content(content)
}

/// Compose a kind:9 reply using C7's NIP-18 `q` schema.
pub fn compose_chat_reply(content: String, reply: ChatReply) -> EventBuilder {
    let relay = reply.relay.to_string();
    let event_id = reply.event_id.to_hex();
    let reply_author = reply.author.to_hex();
    EventBuilder::new(Kind::from(CHAT_KIND))
        .content(content)
        .tag(
            Tag::parse(["q", &event_id, &relay, &reply_author])
                .expect("a typed C7 reply always yields a well-formed q row"),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::Keys;

    fn author() -> PublicKey {
        Keys::generate().public_key()
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

    #[test]
    fn reply_uses_q_and_no_e_p_h_or_previous_rows() {
        let parent_id = EventId::from_slice(&[7; 32]).unwrap();
        let parent_author = author();
        let relay = RelayUrl::parse("wss://chat.example.com").unwrap();
        let event = compose_chat_reply(
            "reply".to_string(),
            ChatReply::new(parent_id, relay.clone(), parent_author),
        );

        assert_eq!(event.kind, Kind::from(CHAT_KIND));
        assert_eq!(
            rows(&event),
            vec![vec![
                "q".to_string(),
                parent_id.to_hex(),
                relay.to_string(),
                parent_author.to_hex(),
            ]]
        );
    }
}
