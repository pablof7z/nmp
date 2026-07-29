//! Compose a publishable NIP-22 `WriteIntent` (#572). This crate owns the
//! comment SCHEMA and this one door owns its write POLICY -- "a NIP-22
//! comment write is `Durable` + `Auto`" lives here and nowhere else, so
//! callers never hand-roll durability or routing. Still no active-account
//! query and no wall-clock read: the engine resolves the identity and stamps
//! the timestamp at acceptance, which is why this whole crate keeps its zero
//! engine dependency without taking either as a parameter.

use nmp_grammar::{
    CorrelationToken, Durability, Identity, WriteIntent, WritePayload, WriteRouting,
};

use crate::build::{compose_comment_reply, compose_top_level_comment};
use crate::root::{CommentParent, CommentRoot};

/// Compose a durable, author-outbox-routed `WriteIntent` for a NIP-22
/// comment on `root`. `parent` selects top-level (mirrors the root) vs.
/// reply (points at another comment event) composition -- see
/// [`crate::compose_top_level_comment`]/[`crate::compose_comment_reply`]
/// for the exact tag shapes. `correlation` is passed straight through to
/// [`WriteIntent::correlation`] (#591) -- this crate adds no
/// comment-specific correlation machinery of its own.
pub fn comment_intent(
    root: &CommentRoot,
    parent: CommentParent,
    content: String,
    correlation: Option<CorrelationToken>,
) -> WriteIntent {
    let builder = match parent {
        CommentParent::Root => compose_top_level_comment(root, content),
        CommentParent::Comment { .. } => compose_comment_reply(root, parent, content),
    };
    WriteIntent {
        payload: WritePayload::Event(builder),
        durability: Durability::Durable,
        routing: WriteRouting::Auto,
        identity: Identity::Active,
        correlation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::Nip73Target;
    use nostr::EventId;

    fn podcast_root() -> CommentRoot {
        CommentRoot::External(Nip73Target::podcast_episode_guid("guid-1").unwrap())
    }

    #[test]
    fn comment_intent_is_a_builder_durable_auto() {
        let intent = comment_intent(&podcast_root(), CommentParent::Root, "hi".to_string(), None);
        assert!(matches!(
            &intent.payload,
            WritePayload::Event(builder) if builder.created_at.is_none()
        ));
        assert_eq!(intent.durability, Durability::Durable);
        assert!(matches!(intent.routing, WriteRouting::Auto));
        assert_eq!(intent.identity, Identity::Active);
        assert!(intent.correlation.is_none());
    }

    /// #591 pass-through: an optional correlation token rides straight
    /// onto the composed intent with no comment-specific machinery.
    #[test]
    fn comment_intent_passes_through_the_correlation_token() {
        let token = CorrelationToken::try_from("nip22-correlation").unwrap();
        let intent = comment_intent(
            &podcast_root(),
            CommentParent::Comment {
                event_id: EventId::from_slice(&[1; 32]).unwrap(),
                author: None,
            },
            "reply".to_string(),
            Some(token.clone()),
        );
        assert_eq!(intent.correlation, Some(token));
    }
}
