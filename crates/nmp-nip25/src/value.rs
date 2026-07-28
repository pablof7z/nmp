use std::fmt;

use nmp::Tag;
use nostr::Url;

/// A validated NIP-25 reaction value.
///
/// The wire content and optional NIP-30 `emoji` tag are private so callers
/// cannot pair an arbitrary `:shortcode:` body with missing or contradictory
/// metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactionValue {
    pub(crate) content: String,
    pub(crate) custom_emoji_tag: Option<Tag>,
}

impl ReactionValue {
    /// NIP-25's canonical like/upvote representation. The alternative empty
    /// wire form is normalized here rather than exposed as a second value.
    pub fn like() -> Self {
        Self {
            content: "+".to_string(),
            custom_emoji_tag: None,
        }
    }

    /// NIP-25's canonical dislike/downvote representation.
    pub fn dislike() -> Self {
        Self {
            content: "-".to_string(),
            custom_emoji_tag: None,
        }
    }

    /// A user-selected Unicode reaction.
    ///
    /// NIP-25 does not define a normative Unicode emoji classifier. This
    /// constructor therefore preserves any non-empty, non-whitespace/control
    /// Unicode token while reserving `+`, `-`, and `:shortcode:` for their
    /// typed constructors.
    pub fn emoji(value: &str) -> Result<Self, ReactionValueError> {
        if value.is_empty() {
            return Err(ReactionValueError::EmptyEmoji);
        }
        if matches!(value, "+" | "-") {
            return Err(ReactionValueError::StandardValueRequiresTypedVariant {
                got: value.to_string(),
            });
        }
        if value.starts_with(':') && value.ends_with(':') {
            return Err(ReactionValueError::CustomEmojiRequiresMetadata {
                got: value.to_string(),
            });
        }
        if value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
        {
            return Err(ReactionValueError::InvalidEmojiToken {
                got: value.to_string(),
            });
        }
        Ok(Self {
            content: value.to_string(),
            custom_emoji_tag: None,
        })
    }

    /// One NIP-30 custom emoji reaction: exactly one `:shortcode:` body and
    /// exactly one matching `emoji` tag.
    pub fn custom_emoji(shortcode: &str, image_url: &str) -> Result<Self, ReactionValueError> {
        if shortcode.is_empty()
            || !shortcode.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
            })
        {
            return Err(ReactionValueError::InvalidCustomEmojiShortcode {
                got: shortcode.to_string(),
            });
        }
        let parsed_url =
            Url::parse(image_url).map_err(|_| ReactionValueError::InvalidCustomEmojiUrl {
                got: image_url.to_string(),
            })?;
        if !matches!(parsed_url.scheme(), "http" | "https") {
            return Err(ReactionValueError::InvalidCustomEmojiUrl {
                got: image_url.to_string(),
            });
        }
        let tag = Tag::parse(["emoji", shortcode, parsed_url.as_str()])
            .expect("validated NIP-30 cells always form one valid emoji tag");
        Ok(Self {
            content: format!(":{shortcode}:"),
            custom_emoji_tag: Some(tag),
        })
    }
}

/// Typed refusal from reaction-value validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReactionValueError {
    EmptyEmoji,
    StandardValueRequiresTypedVariant { got: String },
    CustomEmojiRequiresMetadata { got: String },
    InvalidEmojiToken { got: String },
    InvalidCustomEmojiShortcode { got: String },
    InvalidCustomEmojiUrl { got: String },
}

impl fmt::Display for ReactionValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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

impl std::error::Error for ReactionValueError {}
