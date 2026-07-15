use nmp::{Durability, WriteIntent, WritePayload, WriteRouting};
use nostr::{Event, Kind, PublicKey, RelayUrl, Tag, Timestamp, UnsignedEvent};

const SIMPLE_GROUPS_KIND: u16 = 10009;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayChange {
    Add,
    Remove,
}

pub enum ComposeRelayChangeResult {
    NoChange,
    Publish(Box<WriteIntent>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposeRelayChangeError {
    BaseHasWrongAuthor,
    BaseHasWrongKind,
    TimestampExhausted,
    InvalidGeneratedTag,
}

pub fn contains_relay(event: &Event, relay: &RelayUrl) -> bool {
    event
        .tags
        .iter()
        .any(|tag| relay_from_tag(tag).as_ref() == Some(relay))
}

/// Compose an exact, tag-preserving kind:10009 relay-list edit.
///
/// Existing content and every unrelated tag remain byte-for-byte unchanged.
/// Add appends one canonical public `r` tag. Remove deletes every public `r`
/// tag that parses to the requested canonical relay and leaves `group` tags
/// untouched. A reconciled absent base may create the first list on Add; the
/// returned exact-base precondition is `None`, so a concurrent winner refuses
/// acceptance atomically instead of being overwritten.
pub fn compose_relay_change(
    author: PublicKey,
    base: Option<&Event>,
    relay: &RelayUrl,
    change: RelayChange,
    now: Timestamp,
) -> Result<ComposeRelayChangeResult, ComposeRelayChangeError> {
    if let Some(base) = base {
        if base.pubkey != author {
            return Err(ComposeRelayChangeError::BaseHasWrongAuthor);
        }
        if base.kind != Kind::Custom(SIMPLE_GROUPS_KIND) {
            return Err(ComposeRelayChangeError::BaseHasWrongKind);
        }
    }

    let is_present = base.is_some_and(|event| contains_relay(event, relay));
    if is_present == (change == RelayChange::Add) {
        return Ok(ComposeRelayChangeResult::NoChange);
    }

    let mut tags = base
        .map(|event| event.tags.iter().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    if change == RelayChange::Add {
        tags.push(
            Tag::parse(vec!["r".to_string(), relay.to_string()])
                .map_err(|_| ComposeRelayChangeError::InvalidGeneratedTag)?,
        );
    } else {
        tags.retain(|tag| relay_from_tag(tag).as_ref() != Some(relay));
    }

    let created_at = match base {
        Some(base) if now <= base.created_at => Timestamp::from_secs(
            base.created_at
                .as_secs()
                .checked_add(1)
                .ok_or(ComposeRelayChangeError::TimestampExhausted)?,
        ),
        _ => now,
    };
    let unsigned = UnsignedEvent::new(
        author,
        created_at,
        Kind::Custom(SIMPLE_GROUPS_KIND),
        tags,
        base.map(|event| event.content.clone()).unwrap_or_default(),
    );
    Ok(ComposeRelayChangeResult::Publish(Box::new(WriteIntent {
        payload: WritePayload::UnsignedReplaceableEdit {
            unsigned,
            expected_base: base.map(|event| event.id),
        },
        durability: Durability::Durable,
        routing: WriteRouting::AuthorOutbox,
    })))
}

fn relay_from_tag(tag: &Tag) -> Option<RelayUrl> {
    let values = tag.as_slice();
    if values.first().map(String::as_str) != Some("r") {
        return None;
    }
    values.get(1).and_then(|value| RelayUrl::parse(value).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::{Keys, UnsignedEvent};

    fn event(author: &Keys, at: u64, tags: Vec<Vec<&str>>, content: &str) -> Event {
        let tags = tags
            .into_iter()
            .map(|values| {
                Tag::parse(values.into_iter().map(str::to_string).collect::<Vec<_>>()).unwrap()
            })
            .collect::<Vec<_>>();
        UnsignedEvent::new(
            author.public_key(),
            Timestamp::from_secs(at),
            Kind::Custom(SIMPLE_GROUPS_KIND),
            tags,
            content,
        )
        .sign_with_keys(author)
        .unwrap()
    }

    fn unsigned(intent: &WriteIntent) -> (&UnsignedEvent, Option<nostr::EventId>) {
        let WritePayload::UnsignedReplaceableEdit {
            unsigned,
            expected_base,
        } = &intent.payload
        else {
            panic!("expected guarded replacement")
        };
        (unsigned, *expected_base)
    }

    #[test]
    fn add_preserves_all_fields_and_appends_one_canonical_r_tag() {
        let author = Keys::generate();
        let relay = RelayUrl::parse("wss://new.example").unwrap();
        let base = event(
            &author,
            10,
            vec![
                vec!["group", "alpha", "wss://old.example", "Alpha"],
                vec!["x", "opaque", "tokens"],
            ],
            "encrypted private items",
        );
        let ComposeRelayChangeResult::Publish(intent) = compose_relay_change(
            author.public_key(),
            Some(&base),
            &relay,
            RelayChange::Add,
            Timestamp::from_secs(9),
        )
        .unwrap() else {
            panic!("must publish")
        };
        let (draft, expected) = unsigned(&intent);
        assert_eq!(draft.created_at, Timestamp::from_secs(11));
        assert_eq!(draft.content, base.content);
        let rows = draft
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect::<Vec<_>>();
        let base_rows = base
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect::<Vec<_>>();
        assert_eq!(rows[0], base_rows[0]);
        assert_eq!(rows[1], base_rows[1]);
        assert_eq!(rows[2], vec!["r".to_string(), relay.to_string()]);
        assert_eq!(expected, Some(base.id));
    }

    #[test]
    fn remove_deletes_matching_r_tags_only() {
        let author = Keys::generate();
        let relay = RelayUrl::parse("wss://relay.example").unwrap();
        let base = event(
            &author,
            10,
            vec![
                vec!["r", "wss://relay.example"],
                vec!["group", "alpha", "wss://relay.example", "Alpha"],
                vec!["r", "wss://other.example"],
                vec!["r", "wss://relay.example/"],
            ],
            "keep",
        );
        let ComposeRelayChangeResult::Publish(intent) = compose_relay_change(
            author.public_key(),
            Some(&base),
            &relay,
            RelayChange::Remove,
            Timestamp::from_secs(20),
        )
        .unwrap() else {
            panic!("must publish")
        };
        let (draft, _) = unsigned(&intent);
        let rows = draft
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            vec![
                vec![
                    "group".to_string(),
                    "alpha".to_string(),
                    "wss://relay.example".to_string(),
                    "Alpha".to_string(),
                ],
                vec!["r".to_string(), "wss://other.example".to_string()],
            ]
        );
    }

    #[test]
    fn reconciled_absence_can_create_first_list_with_none_precondition() {
        let author = Keys::generate();
        let relay = RelayUrl::parse("wss://first.example").unwrap();
        let ComposeRelayChangeResult::Publish(intent) = compose_relay_change(
            author.public_key(),
            None,
            &relay,
            RelayChange::Add,
            Timestamp::from_secs(5),
        )
        .unwrap() else {
            panic!("must publish")
        };
        let (draft, expected) = unsigned(&intent);
        assert_eq!(draft.content, "");
        assert_eq!(expected, None);
        assert!(contains_relay(
            &draft.clone().sign_with_keys(&author).unwrap(),
            &relay
        ));
    }

    #[test]
    fn already_satisfied_change_is_a_noop() {
        let author = Keys::generate();
        let relay = RelayUrl::parse("wss://relay.example").unwrap();
        let base = event(&author, 1, vec![vec!["r", "wss://relay.example"]], "");
        assert!(matches!(
            compose_relay_change(
                author.public_key(),
                Some(&base),
                &relay,
                RelayChange::Add,
                Timestamp::from_secs(2)
            ),
            Ok(ComposeRelayChangeResult::NoChange)
        ));
        assert!(matches!(
            compose_relay_change(
                author.public_key(),
                None,
                &relay,
                RelayChange::Remove,
                Timestamp::from_secs(2)
            ),
            Ok(ComposeRelayChangeResult::NoChange)
        ));
    }
}
