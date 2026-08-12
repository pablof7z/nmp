use nmp::{EventBuilder, Identity, WriteIntent, WritePayload, WriteRouting};
use nmp_document_edit::{
    DocumentEditPlan, TagEdit, TagInsertion, TagItemPattern, TagItemSelector, TagRowPattern,
};
use nostr::{Event, EventId, Kind, PublicKey, Tag};

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
    BaseHasWrongKind,
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
///
/// It takes neither an author nor a clock. A base somebody else authored
/// needs no error of its own: the precondition is checked at the editing
/// identity's own coordinate, where a foreign event is never the winner, so
/// it reports through the same conflict door as every other stale base. And
/// the timestamp is decided inside the acceptance transaction as
/// `max(clock, winner + 1)` -- against the row the precondition is holding,
/// which is the only place monotonicity can actually be guaranteed, so
/// there is no arithmetic here left to exhaust.
pub fn compose_follow_change(
    base: &Event,
    target: PublicKey,
    change: FollowChange,
) -> Result<ComposeFollowResult, ComposeFollowError> {
    if base.kind != Kind::ContactList {
        return Err(ComposeFollowError::BaseHasWrongKind);
    }

    let wants_follow = change == FollowChange::Follow;
    let target = target.to_hex();
    let selector = TagItemSelector::one(
        TagItemPattern::new(vec![TagRowPattern::prefix(vec![
            "p".to_string(),
            target.clone(),
        ])
        .map_err(|_| ComposeFollowError::InvalidGeneratedTag)?])
        .map_err(|_| ComposeFollowError::InvalidGeneratedTag)?,
    );
    let edit = if wants_follow {
        TagEdit::ensure_present(
            selector,
            vec![vec!["p".to_string(), target]],
            TagInsertion::end(),
        )
        .map_err(|_| ComposeFollowError::InvalidGeneratedTag)?
    } else {
        TagEdit::remove(selector)
    };
    let plan = DocumentEditPlan::tags(edit);
    // Borrow raw cells for matching; only the final changed document is
    // reconstructed. There is no input-wide string clone before the edit.
    let source = base.tags.iter().map(Tag::as_slice).collect::<Vec<_>>();
    let applied = plan
        .apply_tags(&source)
        .map_err(|_| ComposeFollowError::InvalidGeneratedTag)?;
    let Some(rows) = applied.replacement else {
        return Ok(ComposeFollowResult::NoChange);
    };
    let tags = rows
        .into_iter()
        .map(Tag::parse)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ComposeFollowError::InvalidGeneratedTag)?;

    Ok(ComposeFollowResult::Publish(Box::new(WriteIntent {
        payload: WritePayload::ReplaceableEdit {
            builder: EventBuilder {
                kind: Kind::ContactList,
                tags,
                content: base.content.clone(),
                created_at: None,
            },
            expected_base: Some(base.id),
        },
        routing: WriteRouting::Auto,
        identity: Identity::Active,
        correlation: None,
    })))
}

/// Extract the precondition for tests and adapters without opening up any
/// mutable registry or protocol projection.
pub fn expected_base(intent: &WriteIntent) -> Option<Option<EventId>> {
    match &intent.payload {
        WritePayload::ReplaceableEdit { expected_base, .. } => Some(*expected_base),
        WritePayload::Event(_) | WritePayload::Signed(_) => None,
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
        nostr::UnsignedEvent::new(
            author.public_key(),
            nostr::Timestamp::from_secs(at),
            Kind::ContactList,
            tags,
            content,
        )
        .sign_with_keys(author)
        .unwrap()
    }

    fn composed(intent: &WriteIntent) -> &EventBuilder {
        let WritePayload::ReplaceableEdit { builder, .. } = &intent.payload else {
            panic!("expected replaceable edit")
        };
        builder
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

        let ComposeFollowResult::Publish(intent) =
            compose_follow_change(&base, target.public_key(), FollowChange::Follow).unwrap()
        else {
            panic!("must publish")
        };

        let draft = composed(&intent);
        // Unstated on purpose: the acceptance transaction stamps
        // `max(clock, winner + 1)` against the row it is CAS-ing, which is
        // the only place the winner is actually known.
        assert_eq!(draft.created_at, None);
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
        let ComposeFollowResult::Publish(intent) =
            compose_follow_change(&base, target.public_key(), FollowChange::Unfollow).unwrap()
        else {
            panic!("must publish")
        };
        let actual: Vec<Vec<String>> = composed(&intent)
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
            compose_follow_change(&base, target.public_key(), FollowChange::Follow,),
            Ok(ComposeFollowResult::NoChange)
        ));
        let empty = event(&author, 1, vec![], "");
        assert!(matches!(
            compose_follow_change(&empty, target.public_key(), FollowChange::Unfollow,),
            Ok(ComposeFollowResult::NoChange)
        ));
    }

    /// A base somebody else authored is composed without complaint and gets
    /// no error of its own: the precondition is checked at the editing
    /// identity's own coordinate, where a foreign event is never the winner,
    /// so it is unsatisfiable and reports through the ordinary conflict door
    /// at acceptance instead of a compose-time author comparison.
    #[test]
    fn a_foreign_base_composes_and_is_left_to_the_precondition() {
        let wrong = Keys::generate();
        let target = Keys::generate();
        let wrong_author = event(&wrong, 1, vec![], "");
        let ComposeFollowResult::Publish(intent) =
            compose_follow_change(&wrong_author, target.public_key(), FollowChange::Follow)
                .unwrap()
        else {
            panic!("must publish")
        };
        assert_eq!(expected_base(&intent), Some(Some(wrong_author.id)));
    }

    #[test]
    fn base_validation_fails_closed() {
        let author = Keys::generate();
        let target = Keys::generate();
        let mut wrong_kind = event(&author, 1, vec![], "");
        wrong_kind.kind = Kind::TextNote;
        assert_eq!(
            compose_follow_change(&wrong_kind, target.public_key(), FollowChange::Follow,).err(),
            Some(ComposeFollowError::BaseHasWrongKind)
        );
    }
}
