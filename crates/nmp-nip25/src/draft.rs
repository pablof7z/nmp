use std::fmt;

use nmp::{Engine, PublicKey, Timestamp, UnsignedEvent};
use nostr::EventBuilder;

use crate::{ReactionTarget, ReactionValue};

/// A complete, immutable unsigned NIP-25 draft.
///
/// Direct Rust composition may pass this value into another closed protocol
/// context such as NIP-29. It carries no routing, signing, persistence,
/// receipt, retry, or publication claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactionDraft {
    event: UnsignedEvent,
}

impl ReactionDraft {
    pub fn event(&self) -> &UnsignedEvent {
        &self.event
    }

    pub fn into_event(self) -> UnsignedEvent {
        self.event
    }
}

/// Failure to compose a draft from engine-owned identity state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReactionDraftError {
    EngineClosed,
    NoActiveReactionAuthor,
}

impl fmt::Display for ReactionDraftError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EngineClosed => f.write_str("engine already shut down"),
            Self::NoActiveReactionAuthor => {
                f.write_str("NIP-25 draft requires an active reaction author")
            }
        }
    }
}

impl std::error::Error for ReactionDraftError {}

pub(crate) fn compose_reaction_at(
    target: &ReactionTarget,
    value: ReactionValue,
    author: PublicKey,
    created_at: Timestamp,
) -> ReactionDraft {
    let mut builder =
        EventBuilder::reaction(target.inner.clone(), value.content).custom_created_at(created_at);
    if let Some(tag) = value.custom_emoji_tag {
        builder = builder.tag(tag);
    }
    ReactionDraft {
        event: builder.build(author),
    }
}

/// Compose a complete unsigned kind:7 draft from semantic inputs.
///
/// The active author is read from `engine`; the event time is read in Rust.
/// Native callers cannot provide either value. The returned draft remains
/// orthogonal to routing and publication.
pub fn reaction_draft(
    engine: &Engine,
    target: &ReactionTarget,
    value: ReactionValue,
) -> Result<ReactionDraft, ReactionDraftError> {
    let author = engine
        .active_account()
        .map_err(|_| ReactionDraftError::EngineClosed)?
        .ok_or(ReactionDraftError::NoActiveReactionAuthor)?;
    Ok(compose_reaction_at(target, value, author, Timestamp::now()))
}
