//! Opaque native projection of the NIP-25 target and draft boundary (#155).
//!
//! Native callers identify an existing event, choose one typed reaction
//! value, and receive opaque objects. The canonical target schema and
//! unsigned event bytes are Rust-owned; this module exports no raw kind,
//! tags, author, time, relay routing, write intent, or publication operation.

use std::fmt;
use std::sync::Arc;

use crate::protocol::FfiProtocolDraft;

/// A target qualified from one exact event in NMP's canonical store.
///
/// The inner NIP-25 schema is intentionally unavailable through UniFFI.
#[derive(uniffi::Object)]
pub struct FfiReactionTarget {
    pub(crate) inner: nmp_nip25::ReactionTarget,
}

/// Native semantic input for one reaction. Standard values and NIP-30
/// custom emoji cannot be smuggled through an unvalidated raw content field.
#[derive(uniffi::Enum, Clone, Debug)]
pub enum FfiReactionValue {
    Like,
    Dislike,
    Emoji {
        value: String,
    },
    CustomEmoji {
        shortcode: String,
        image_url: String,
    },
}

/// Typed failures from canonical target qualification and draft composition.
#[derive(uniffi::Error, Clone, Debug, PartialEq, Eq)]
pub enum FfiReactionError {
    InvalidEventId { got: String },
    TargetNotFound { event_id: String },
    TargetNotVerified { event_id: String },
    CanonicalLookupUnavailable { reason: String },
    EngineClosed,
    NoActiveReactionAuthor,
    EmptyEmoji,
    StandardValueRequiresTypedVariant { got: String },
    CustomEmojiRequiresMetadata { got: String },
    InvalidEmojiToken { got: String },
    InvalidCustomEmojiShortcode { got: String },
    InvalidCustomEmojiUrl { got: String },
}

impl fmt::Display for FfiReactionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEventId { got } => write!(f, "invalid Nostr event id: {got:?}"),
            Self::TargetNotFound { event_id } => {
                write!(f, "event {event_id} is not in the canonical NMP store")
            }
            Self::TargetNotVerified { event_id } => {
                write!(f, "canonical row {event_id} is not a verified signed event")
            }
            Self::CanonicalLookupUnavailable { reason } => {
                write!(f, "canonical target lookup unavailable: {reason}")
            }
            Self::EngineClosed => f.write_str("engine already shut down"),
            Self::NoActiveReactionAuthor => {
                f.write_str("NIP-25 draft requires an active reaction author")
            }
            Self::EmptyEmoji => f.write_str("Unicode reaction must not be empty"),
            Self::StandardValueRequiresTypedVariant { got } => {
                write!(f, "{got:?} must use the typed like/dislike variant")
            }
            Self::CustomEmojiRequiresMetadata { got } => {
                write!(f, "{got:?} requires matching typed NIP-30 metadata")
            }
            Self::InvalidEmojiToken { got } => {
                write!(f, "{got:?} contains whitespace or control characters")
            }
            Self::InvalidCustomEmojiShortcode { got } => write!(
                f,
                "custom emoji shortcode {got:?} must use ASCII letters, digits, '-' or '_'"
            ),
            Self::InvalidCustomEmojiUrl { got } => {
                write!(f, "custom emoji image URL is not HTTP(S): {got:?}")
            }
        }
    }
}

fn target_error_to_ffi(error: nmp_nip25::ReactionTargetError) -> FfiReactionError {
    match error {
        nmp_nip25::ReactionTargetError::EngineClosed => FfiReactionError::EngineClosed,
        nmp_nip25::ReactionTargetError::CanonicalLookupUnavailable { reason } => {
            FfiReactionError::CanonicalLookupUnavailable { reason }
        }
        nmp_nip25::ReactionTargetError::TargetNotFound { event_id } => {
            FfiReactionError::TargetNotFound {
                event_id: event_id.to_hex(),
            }
        }
        nmp_nip25::ReactionTargetError::TargetNotVerified { event_id } => {
            FfiReactionError::TargetNotVerified {
                event_id: event_id.to_hex(),
            }
        }
    }
}

fn draft_error_to_ffi(error: nmp_nip25::ReactionDraftError) -> FfiReactionError {
    match error {
        nmp_nip25::ReactionDraftError::EngineClosed => FfiReactionError::EngineClosed,
        nmp_nip25::ReactionDraftError::NoActiveReactionAuthor => {
            FfiReactionError::NoActiveReactionAuthor
        }
    }
}

fn value_from_ffi(value: FfiReactionValue) -> Result<nmp_nip25::ReactionValue, FfiReactionError> {
    let value = match value {
        FfiReactionValue::Like => Ok(nmp_nip25::ReactionValue::like()),
        FfiReactionValue::Dislike => Ok(nmp_nip25::ReactionValue::dislike()),
        FfiReactionValue::Emoji { value } => nmp_nip25::ReactionValue::emoji(&value),
        FfiReactionValue::CustomEmoji {
            shortcode,
            image_url,
        } => nmp_nip25::ReactionValue::custom_emoji(&shortcode, &image_url),
    };
    value.map_err(|error| match error {
        nmp_nip25::ReactionValueError::EmptyEmoji => FfiReactionError::EmptyEmoji,
        nmp_nip25::ReactionValueError::StandardValueRequiresTypedVariant { got } => {
            FfiReactionError::StandardValueRequiresTypedVariant { got }
        }
        nmp_nip25::ReactionValueError::CustomEmojiRequiresMetadata { got } => {
            FfiReactionError::CustomEmojiRequiresMetadata { got }
        }
        nmp_nip25::ReactionValueError::InvalidEmojiToken { got } => {
            FfiReactionError::InvalidEmojiToken { got }
        }
        nmp_nip25::ReactionValueError::InvalidCustomEmojiShortcode { got } => {
            FfiReactionError::InvalidCustomEmojiShortcode { got }
        }
        nmp_nip25::ReactionValueError::InvalidCustomEmojiUrl { got } => {
            FfiReactionError::InvalidCustomEmojiUrl { got }
        }
    })
}

pub(crate) fn reaction_target(
    engine: &nmp::Engine,
    event_id: String,
) -> Result<Arc<FfiReactionTarget>, FfiReactionError> {
    let event_id = nmp::EventId::parse(&event_id)
        .map_err(|_| FfiReactionError::InvalidEventId { got: event_id })?;
    let target = nmp_nip25::reaction_target(engine, event_id).map_err(target_error_to_ffi)?;
    Ok(Arc::new(FfiReactionTarget { inner: target }))
}

pub(crate) fn reaction_draft(
    engine: &nmp::Engine,
    target: Arc<FfiReactionTarget>,
    value: FfiReactionValue,
) -> Result<Arc<FfiProtocolDraft>, FfiReactionError> {
    let value = value_from_ffi(value)?;
    let draft =
        nmp_nip25::reaction_draft(engine, &target.inner, value).map_err(draft_error_to_ffi)?;
    Ok(Arc::new(FfiProtocolDraft::new(draft.into_event())))
}

#[cfg(test)]
#[path = "nip25_tests.rs"]
mod tests;
