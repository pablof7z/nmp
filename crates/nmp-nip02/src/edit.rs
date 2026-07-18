use nmp::{Durability, WriteIntent, WritePayload, WriteRouting};
use nostr::{Event, EventId, Kind, PublicKey, Tag, Timestamp, UnsignedEvent};

/// The requested relationship after a NIP-02 edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowChange {
    Follow,
    Unfollow,
}

/// A pure edit either proves the contact list already has the requested
/// relationship or returns one closed, compare-and-swap write intent.
pub enum ComposeFollowResult {
    NoChange,
    Publish(Box<WriteIntent>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposeFollowError {
    BaseHasWrongAuthor,
    BaseHasWrongKind,
    TimestampExhausted,
    InvalidGeneratedTag,
}

/// True when `event` contains any NIP-02 `p` tag for `target`. Relay hints,
/// petnames, extra tag fields, malformed unrelated tags, and ordering are
/// deliberately irrelevant to membership and remain untouched by edits.
pub fn follows(event: &Event, target: PublicKey) -> bool {
    let target = target.to_hex();
    event.tags.iter().any(|tag| {
        let values = tag.as_slice();
        values.first().map(String::as_str) == Some("p")
            && values.get(1).map(String::as_str) == Some(target.as_str())
    })
}

/// Compose a NIP-02 whole-list replacement from an exact local base.
///
/// Every tag and the content string are preserved byte-for-byte and in the
/// same order except for the requested target: follow appends one minimal
/// `p` tag (NIP-02's chronological convention), while unfollow removes all
/// matching `p` tags. The returned payload carries `base.id` as an atomic
/// acceptance precondition; a concurrent winner produces a typed conflict
/// before any write is journaled. This ordinary edit requires an established
/// base. Creating a first contact list needs a separately named policy and
/// cannot masquerade as `follow`.
pub fn compose_follow_change(
    author: PublicKey,
    base: &Event,
    target: PublicKey,
    change: FollowChange,
    now: Timestamp,
) -> Result<ComposeFollowResult, ComposeFollowError> {
    if base.pubkey != author {
        return Err(ComposeFollowError::BaseHasWrongAuthor);
    }
    if base.kind != Kind::ContactList {
        return Err(ComposeFollowError::BaseHasWrongKind);
    }

    let currently_follows = follows(base, target);
    let wants_follow = change == FollowChange::Follow;
    if currently_follows == wants_follow {
        return Ok(ComposeFollowResult::NoChange);
    }

    let mut tags: Vec<Tag> = base.tags.iter().cloned().collect();
    if wants_follow {
        let tag = Tag::parse(vec!["p".to_string(), target.to_hex()])
            .map_err(|_| ComposeFollowError::InvalidGeneratedTag)?;
        tags.push(tag);
    } else {
        let target = target.to_hex();
        tags.retain(|tag| {
            let values = tag.as_slice();
            !(values.first().map(String::as_str) == Some("p")
                && values.get(1).map(String::as_str) == Some(target.as_str()))
        });
    }

    let created_at = if now <= base.created_at {
        Timestamp::from_secs(
            base.created_at
                .as_secs()
                .checked_add(1)
                .ok_or(ComposeFollowError::TimestampExhausted)?,
        )
    } else {
        now
    };
    let unsigned = UnsignedEvent::new(
        author,
        created_at,
        Kind::ContactList,
        tags,
        base.content.clone(),
    );
    Ok(ComposeFollowResult::Publish(Box::new(WriteIntent {
        payload: WritePayload::UnsignedReplaceableEdit {
            unsigned,
            expected_base: Some(base.id),
        },
        durability: Durability::Durable,
        routing: WriteRouting::AuthorOutbox,
        identity_override: None,
        correlation: None,
    })))
}

/// Extract the precondition for tests and adapters without opening up any
/// mutable registry or protocol projection.
pub fn expected_base(intent: &WriteIntent) -> Option<Option<EventId>> {
    match &intent.payload {
        WritePayload::UnsignedReplaceableEdit { expected_base, .. } => Some(*expected_base),
        WritePayload::Unsigned(_) | WritePayload::Signed(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::Keys;

    fn event(author: &Keys, at: u64, raw_tags: Vec<Vec<&str>>, content: &str) -> Event {
        let tags = raw_tags
            .into_iter()
            .map(|values| {
                Tag::parse(values.into_iter().map(str::to_string).collect::<Vec<_>>()).unwrap()
            })
            .collect::<Vec<_>>();
        UnsignedEvent::new(
            author.public_key(),
            Timestamp::from_secs(at),
            Kind::ContactList,
            tags,
            content,
        )
        .sign_with_keys(author)
        .unwrap()
    }

    fn unsigned(intent: &WriteIntent) -> &UnsignedEvent {
        let WritePayload::UnsignedReplaceableEdit { unsigned, .. } = &intent.payload else {
            panic!("expected replaceable edit")
        };
        unsigned
    }

    #[test]
    fn follow_appends_and_preserves_every_existing_field() {
        let author = Keys::generate();
        let existing = Keys::generate();
        let target = Keys::generate();
        let base = event(
            &author,
            10,
            vec![
                vec!["client", "keep-me"],
                vec![
                    "p",
                    &existing.public_key().to_hex(),
                    "wss://hint.example",
                    "pet",
                ],
                vec!["x", "opaque", "tokens"],
            ],
            "legacy content must survive",
        );

        let ComposeFollowResult::Publish(intent) = compose_follow_change(
            author.public_key(),
            &base,
            target.public_key(),
            FollowChange::Follow,
            Timestamp::from_secs(9),
        )
        .unwrap() else {
            panic!("must publish")
        };

        let draft = unsigned(&intent);
        assert_eq!(draft.created_at, Timestamp::from_secs(11));
        assert_eq!(draft.content, base.content);
        let actual: Vec<Vec<String>> = draft.tags.iter().map(|t| t.as_slice().to_vec()).collect();
        let mut expected: Vec<Vec<String>> =
            base.tags.iter().map(|t| t.as_slice().to_vec()).collect();
        expected.push(vec!["p".into(), target.public_key().to_hex()]);
        assert_eq!(actual, expected);
        assert_eq!(expected_base(&intent), Some(Some(base.id)));
    }

    #[test]
    fn unfollow_removes_all_target_tags_only_and_keeps_order() {
        let author = Keys::generate();
        let target = Keys::generate();
        let other = Keys::generate();
        let target_hex = target.public_key().to_hex();
        let other_hex = other.public_key().to_hex();
        let base = event(
            &author,
            20,
            vec![
                vec!["p", &target_hex, "wss://one", "one"],
                vec!["x", "keep"],
                vec!["p", &other_hex, "wss://other", "friend"],
                vec!["p", &target_hex, "wss://two", "two"],
            ],
            "",
        );
        let ComposeFollowResult::Publish(intent) = compose_follow_change(
            author.public_key(),
            &base,
            target.public_key(),
            FollowChange::Unfollow,
            Timestamp::from_secs(30),
        )
        .unwrap() else {
            panic!("must publish")
        };
        let actual: Vec<Vec<String>> = unsigned(&intent)
            .tags
            .iter()
            .map(|t| t.as_slice().to_vec())
            .collect();
        assert_eq!(
            actual,
            vec![
                vec!["x".into(), "keep".into()],
                vec!["p".into(), other_hex, "wss://other".into(), "friend".into()]
            ]
        );
    }

    #[test]
    fn already_requested_relationship_is_a_noop() {
        let author = Keys::generate();
        let target = Keys::generate();
        let target_hex = target.public_key().to_hex();
        let base = event(&author, 1, vec![vec!["p", &target_hex]], "");
        assert!(matches!(
            compose_follow_change(
                author.public_key(),
                &base,
                target.public_key(),
                FollowChange::Follow,
                Timestamp::from_secs(2)
            ),
            Ok(ComposeFollowResult::NoChange)
        ));
        let empty = event(&author, 1, vec![], "");
        assert!(matches!(
            compose_follow_change(
                author.public_key(),
                &empty,
                target.public_key(),
                FollowChange::Unfollow,
                Timestamp::from_secs(2)
            ),
            Ok(ComposeFollowResult::NoChange)
        ));
    }

    #[test]
    fn base_validation_fails_closed() {
        let author = Keys::generate();
        let wrong = Keys::generate();
        let target = Keys::generate();
        let wrong_author = event(&wrong, 1, vec![], "");
        assert_eq!(
            compose_follow_change(
                author.public_key(),
                &wrong_author,
                target.public_key(),
                FollowChange::Follow,
                Timestamp::from_secs(2)
            )
            .err(),
            Some(ComposeFollowError::BaseHasWrongAuthor)
        );

        let mut wrong_kind = event(&author, 1, vec![], "");
        wrong_kind.kind = Kind::TextNote;
        assert_eq!(
            compose_follow_change(
                author.public_key(),
                &wrong_kind,
                target.public_key(),
                FollowChange::Follow,
                Timestamp::from_secs(2)
            )
            .err(),
            Some(ComposeFollowError::BaseHasWrongKind)
        );
    }
}
