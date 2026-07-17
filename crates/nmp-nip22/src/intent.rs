//! Compose a publishable NIP-22 `WriteIntent` (#572). This is the ONE place
//! that knows "a NIP-22 comment write is `Unsigned` + `Durable` +
//! `AuthorOutbox`" -- callers never hand-roll durability or routing.
//! `author` and `created_at` are explicit caller-supplied parameters (this
//! issue's own design decision): no active-account query, no wall-clock
//! read, hence zero engine dependency for this whole crate.

use nostr::{PublicKey, Timestamp};

use nmp_grammar::{CorrelationToken, Durability, WriteIntent, WritePayload, WriteRouting};

use crate::build::{compose_comment_reply, compose_top_level_comment};
use crate::root::{CommentParent, CommentRoot};

/// Compose a durable, author-outbox-routed `WriteIntent` for a NIP-22
/// comment on `root`. `parent` selects top-level (mirrors the root) vs.
/// reply (points at another comment event) composition -- see
/// [`crate::compose_top_level_comment`]/[`crate::compose_comment_reply`]
/// for the exact tag shapes. `correlation` is passed straight through to
/// [`WriteIntent::correlation`] (#591) -- this crate adds no
/// comment-specific correlation machinery of its own.
#[allow(clippy::too_many_arguments)]
pub fn comment_intent(
    root: &CommentRoot,
    parent: CommentParent,
    author: PublicKey,
    created_at: Timestamp,
    content: String,
    correlation: Option<CorrelationToken>,
) -> WriteIntent {
    let unsigned = match parent {
        CommentParent::Root => compose_top_level_comment(root, author, created_at, content),
        CommentParent::Comment { .. } => {
            compose_comment_reply(root, parent, author, created_at, content)
        }
    };
    WriteIntent {
        payload: WritePayload::Unsigned(unsigned),
        durability: Durability::Durable,
        routing: WriteRouting::AuthorOutbox,
        identity_override: None,
        correlation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::Nip73Target;
    use nostr::{EventId, Keys};

    fn podcast_root() -> CommentRoot {
        CommentRoot::External(Nip73Target::podcast_episode_guid("guid-1").unwrap())
    }

    #[test]
    fn comment_intent_is_unsigned_durable_author_outbox() {
        let author = Keys::generate().public_key();
        let intent = comment_intent(
            &podcast_root(),
            CommentParent::Root,
            author,
            Timestamp::from(1000u64),
            "hi".to_string(),
            None,
        );
        assert!(matches!(intent.payload, WritePayload::Unsigned(_)));
        assert_eq!(intent.durability, Durability::Durable);
        assert!(matches!(intent.routing, WriteRouting::AuthorOutbox));
        assert!(intent.identity_override.is_none());
        assert!(intent.correlation.is_none());
    }

    /// #591 pass-through: an optional correlation token rides straight
    /// onto the composed intent with no comment-specific machinery.
    #[test]
    fn comment_intent_passes_through_the_correlation_token() {
        let author = Keys::generate().public_key();
        let token = CorrelationToken::try_from("nip22-correlation").unwrap();
        let intent = comment_intent(
            &podcast_root(),
            CommentParent::Comment {
                event_id: EventId::from_slice(&[1; 32]).unwrap(),
                author: None,
            },
            author,
            Timestamp::from(1000u64),
            "reply".to_string(),
            Some(token.clone()),
        );
        assert_eq!(intent.correlation, Some(token));
    }
}
