//! Typed kind:9 group-message composition (#156). This sits deliberately
//! above [`crate::compose_group_send`]: product callers provide semantic
//! recipients and an optional reply parent, never an author, wall-clock,
//! event kind, or raw `p`/`e` tag rows.

use nostr::{EventId, PublicKey, RelayUrl, Tag, Timestamp, ToBech32};

use nmp::{Engine, EngineError, WriteIntent};

use crate::send::compose_group_send_with_tags;
use crate::GroupTimelineEvidence;

/// The exact event and author a kind:9 group message replies to. The author
/// is carried both in the marked `e` row (the NIP-10-style outbox hint) and
/// in the deduplicated recipient `p` rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupReplyParent {
    pub event_id: EventId,
    pub author: PublicKey,
}

/// Synchronous failures before an opaque write intent can exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupMessageError {
    /// The engine was shut down while the operation read its active account.
    Engine(EngineError),
    /// A kind:9 unsigned message requires a selected account/author.
    SignedOut,
}

impl From<EngineError> for GroupMessageError {
    fn from(value: EngineError) -> Self {
        Self::Engine(value)
    }
}

/// Compose an ordinary NIP-29 kind:9 message from semantic app inputs.
///
/// NMP owns every protocol-bearing transformation:
///
/// - the active account becomes the unsigned event author;
/// - [`Timestamp::now`] supplies event time inside Rust;
/// - recipient pubkeys are deduplicated in first-selection order, rendered as
///   `nostr:npub…` tokens before `content`, and emitted as `p` tags;
/// - a reply parent contributes a marked `e` row and its author contributes a
///   deduplicated `p` row;
/// - the lower-level composer adds `h`/`previous`, durable pinned-host routing,
///   and the ordinary signing/receipt path consumes the result.
///
/// The reply author's `p` row does not independently add a content mention:
/// reply UIs select that author as an ordinary recipient when they want the
/// visible `nostr:npub…` token. This keeps notification and authored-content
/// policy distinct while still making the reply protocol-correct if a caller
/// omits that redundant selection.
pub fn compose_group_message(
    engine: &Engine,
    host: RelayUrl,
    group_id: &str,
    content: String,
    recipients: Vec<PublicKey>,
    reply_to: Option<GroupReplyParent>,
    previous: &GroupTimelineEvidence,
) -> Result<WriteIntent, GroupMessageError> {
    let author = engine
        .active_account()?
        .ok_or(GroupMessageError::SignedOut)?;

    let mut ordered_recipients = Vec::with_capacity(recipients.len());
    for recipient in recipients {
        if !ordered_recipients.contains(&recipient) {
            ordered_recipients.push(recipient);
        }
    }

    let content = materialize_content(content, &ordered_recipients);

    let mut notification_recipients = ordered_recipients;
    if let Some(parent) = reply_to {
        if !notification_recipients.contains(&parent.author) {
            notification_recipients.push(parent.author);
        }
    }

    let mut tags: Vec<Tag> = notification_recipients
        .into_iter()
        .map(Tag::public_key)
        .collect();
    if let Some(parent) = reply_to {
        tags.push(
            Tag::parse([
                "e".to_string(),
                parent.event_id.to_hex(),
                String::new(),
                "reply".to_string(),
                parent.author.to_hex(),
            ])
            .expect("a canonical event id and pubkey always form a valid marked e tag"),
        );
    }

    Ok(compose_group_send_with_tags(
        host,
        group_id,
        author,
        Timestamp::now(),
        9,
        content,
        tags,
        previous,
    ))
}

fn materialize_content(content: String, recipients: &[PublicKey]) -> String {
    if recipients.is_empty() {
        return content;
    }

    let mentions = recipients
        .iter()
        .map(|pubkey| {
            format!(
                "nostr:{}",
                pubkey
                    .to_bech32()
                    .expect("a canonical 32-byte public key always encodes as npub")
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    if content.is_empty() {
        mentions
    } else {
        format!("{mentions} {content}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nmp::{EngineConfig, WritePayload, WriteRouting};
    use nostr::Keys;

    fn host() -> RelayUrl {
        RelayUrl::parse("wss://group-host.example.com").unwrap()
    }

    fn unsigned(intent: &WriteIntent) -> &nostr::UnsignedEvent {
        let WritePayload::Unsigned(unsigned) = &intent.payload else {
            panic!("group messages must be unsigned")
        };
        unsigned
    }

    #[test]
    fn signed_out_cannot_compose_a_group_message() {
        let engine = Engine::new(EngineConfig::default()).unwrap();
        let result = compose_group_message(
            &engine,
            host(),
            "group-a",
            "hello".to_string(),
            vec![],
            None,
            &GroupTimelineEvidence::none(),
        );
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("a signed-out engine must not compose an intent"),
        };
        assert_eq!(error, GroupMessageError::SignedOut);
        engine.shutdown();
    }

    #[test]
    fn semantic_message_owns_mentions_reply_tags_author_kind_and_time() {
        let engine = Engine::new(EngineConfig::default()).unwrap();
        let author = Keys::generate().public_key();
        let first = Keys::generate().public_key();
        let second = Keys::generate().public_key();
        engine.set_active_account(Some(author)).unwrap();
        let parent_id = EventId::from_slice(&[7; 32]).unwrap();

        let before = Timestamp::now().as_secs();
        let intent = compose_group_message(
            &engine,
            host(),
            "group-a",
            "hello".to_string(),
            vec![first, first, second],
            Some(GroupReplyParent {
                event_id: parent_id,
                author: first,
            }),
            &GroupTimelineEvidence::none(),
        )
        .unwrap();
        let after = Timestamp::now().as_secs();
        let unsigned = unsigned(&intent);

        assert_eq!(unsigned.pubkey, author);
        assert_eq!(unsigned.kind, nostr::Kind::from(9u16));
        assert!((before..=after).contains(&unsigned.created_at.as_secs()));
        assert_eq!(
            unsigned.content,
            format!(
                "nostr:{} nostr:{} hello",
                first.to_bech32().unwrap(),
                second.to_bech32().unwrap()
            )
        );
        let rows = unsigned
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            vec![
                vec!["p".to_string(), first.to_hex()],
                vec!["p".to_string(), second.to_hex()],
                vec![
                    "e".to_string(),
                    parent_id.to_hex(),
                    String::new(),
                    "reply".to_string(),
                    first.to_hex(),
                ],
                vec!["h".to_string(), "group-a".to_string()],
            ]
        );
        assert!(matches!(intent.routing, WriteRouting::PinnedHost(_)));
        engine.shutdown();
    }

    #[test]
    fn reply_author_is_notified_even_without_a_visible_mention_selection() {
        let engine = Engine::new(EngineConfig::default()).unwrap();
        engine
            .set_active_account(Some(Keys::generate().public_key()))
            .unwrap();
        let reply_author = Keys::generate().public_key();
        let parent_id = EventId::from_slice(&[9; 32]).unwrap();

        let intent = compose_group_message(
            &engine,
            host(),
            "group-a",
            "plain body".to_string(),
            vec![],
            Some(GroupReplyParent {
                event_id: parent_id,
                author: reply_author,
            }),
            &GroupTimelineEvidence::none(),
        )
        .unwrap();
        let unsigned = unsigned(&intent);

        assert_eq!(unsigned.content, "plain body");
        let first_tag = unsigned.tags.iter().next().expect("reply p tag");
        assert_eq!(
            first_tag.as_slice(),
            &["p".to_string(), reply_author.to_hex()]
        );
        engine.shutdown();
    }
}
