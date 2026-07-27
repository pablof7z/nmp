//! `nmp-nipc7` -- the pure NIP-C7 kind:9 chat schema owner (#838).
//!
//! C7 owns the event kind and its NIP-18 `q` reply row. It does not own
//! NIP-29 group context, NIP-27 inline mention materialization, or client
//! notification policy. The builders return complete unsigned drafts and
//! never sign, route, publish, or touch engine state.

use nostr::{EventId, Kind, PublicKey, RelayUrl, Tag, Timestamp, UnsignedEvent};

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

/// Build a complete unsigned kind:9 chat draft with no policy-added tags.
pub fn compose_chat(author: PublicKey, created_at: Timestamp, content: String) -> UnsignedEvent {
    UnsignedEvent::new(
        author,
        created_at,
        Kind::from(CHAT_KIND),
        Vec::new(),
        content,
    )
}

/// Build a complete unsigned kind:9 reply using C7's NIP-18 `q` schema.
pub fn compose_chat_reply(
    author: PublicKey,
    created_at: Timestamp,
    content: String,
    reply: ChatReply,
) -> UnsignedEvent {
    let relay = reply.relay.to_string();
    let event_id = reply.event_id.to_hex();
    let reply_author = reply.author.to_hex();
    UnsignedEvent::new(
        author,
        created_at,
        Kind::from(CHAT_KIND),
        vec![Tag::parse(["q", &event_id, &relay, &reply_author])
            .expect("a typed C7 reply always yields a well-formed q row")],
        content,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::Keys;

    fn author() -> PublicKey {
        Keys::generate().public_key()
    }

    fn time() -> Timestamp {
        Timestamp::from(1_700_000_000u64)
    }

    fn rows(event: &UnsignedEvent) -> Vec<Vec<String>> {
        event
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect()
    }

    #[test]
    fn chat_is_kind_9_without_group_or_notification_policy() {
        let event = compose_chat(author(), time(), "hello".to_string());
        assert_eq!(event.kind, Kind::from(CHAT_KIND));
        assert_eq!(event.content, "hello");
        assert!(event.tags.is_empty());
    }

    #[test]
    fn reply_uses_q_and_no_e_p_h_or_previous_rows() {
        let parent_id = EventId::from_slice(&[7; 32]).unwrap();
        let parent_author = author();
        let relay = RelayUrl::parse("wss://chat.example.com").unwrap();
        let event = compose_chat_reply(
            author(),
            time(),
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
