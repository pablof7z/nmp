//! `nmp-nip25` -- the NIP-25 reaction schema owner (#155).
//!
//! A reaction is *"a `kind 7` event that is used to indicate user reactions to
//! other events"*. NIP-25 defines exactly three things and this crate owns
//! exactly those three:
//!
//! 1. **The kind.** 7.
//! 2. **What the content means.** *"A reaction with `content` set to `+` or an
//!    empty string MUST be interpreted as a 'like' or 'upvote'. A reaction with
//!    `content` set to `-` MUST be interpreted as a 'dislike' or 'downvote'. A
//!    reaction with `content` set to an emoji or NIP-30 custom emoji SHOULD NOT
//!    be interpreted as a 'like' or 'dislike'."* [`Reaction`] is those three
//!    cases and nothing else; "like" and "dislike" are the spec's own words,
//!    not a category invented here.
//! 3. **What it points at.** *"There MUST be always an `e` tag set to the `id`
//!    of the event that is being reacted to"*, *"There SHOULD be a `p` tag set
//!    to the `pubkey` of the event being reacted to"*, and the reaction *"MAY
//!    include a `k` tag with the stringified kind number of the reacted
//!    event"*. All three come from the one tagging door, so the letter, the
//!    relay hint, the author slot and the companion `p` row cannot drift
//!    between a reaction and any other pointer in NMP.
//!
//! It owns no write policy: [`react`] returns an [`EventBuilder`], names no
//! author and reads no clock, exactly like every other schema composer here.
//!
//! ## What this crate deliberately does NOT own
//!
//! **NIP-25's kind:17.** *"If the target of a reaction is not a native nostr
//! event, the reaction MUST be a `kind 17` event and MUST include NIP-73
//! external content `k` + `i` tags."* That kind is NIP-25's and belongs here
//! when it can be reached: no [`nmp_nip73::Nip73`](https://docs.rs/) value
//! crosses the native boundary today, so a kind:17 arm would be a variant no
//! caller could construct. It is tracked rather than written blind.
//!
//! **NIP-30 custom emoji.** NIP-25 says *"the client may specify a custom emoji
//! `:shortcode:` in the reaction content"* and that the client *"should refer
//! to the emoji tag"* -- that is a token and its row, the same pairing
//! `nmp_grammar::text!` exists for, and it needs the same one-statement
//! treatment rather than a `String` a caller fills in. So [`Reaction::emoji`]
//! **refuses** a `:shortcode:`-shaped value rather than emitting content no
//! reader can render.

use nmp_grammar::{entity_rows, EventBuilder, TagOptions};
use nostr::{Event, Kind, RelayUrl};

/// NIP-25's kind for a reaction to a Nostr event.
pub const REACTION_KIND: u16 = 7;

/// What a reaction SAYS, in the three readings NIP-25 itself defines.
///
/// Not a `String`: the spec assigns `+`, `-` and the empty string fixed
/// meanings, so a caller passing content by hand can write "like" three ways
/// and "dislike" one way it did not intend. The renderings are
/// [`Self::content`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reaction {
    /// NIP-25: *"MUST be interpreted as a 'like' or 'upvote'"*. Rendered `+`.
    ///
    /// The empty string means this too, and is deliberately not a second
    /// spelling of it: one meaning, one rendering.
    Like,
    /// NIP-25: *"MUST be interpreted as a 'dislike' or 'downvote'"*. Rendered
    /// `-`.
    Dislike,
    /// An emoji, which NIP-25 says *"SHOULD NOT be interpreted as a 'like' or
    /// 'dislike'"* -- it is the reaction itself, shown as written. Built
    /// through [`Self::emoji`], which is where the two states that would
    /// silently mean something else are refused.
    Emoji(String),
}

impl Reaction {
    /// An emoji reaction, or a typed refusal.
    ///
    /// Two refusals, both of which would otherwise publish an event whose
    /// content says something the caller did not:
    ///
    /// - **Empty.** NIP-25 gives the empty string the meaning of `+`, so an
    ///   empty "emoji" is not a neutral reaction, it is a LIKE. A UI that let
    ///   a picker return nothing would silently upvote.
    /// - **`:shortcode:`-shaped.** NIP-25's custom emoji needs a companion
    ///   NIP-30 `emoji` row, which this door does not write, so the content
    ///   would render as literal colons in every client.
    pub fn emoji(emoji: impl Into<String>) -> Result<Self, ReactionError> {
        let emoji = emoji.into();
        if emoji.is_empty() {
            return Err(ReactionError::EmptyEmoji);
        }
        if is_shortcode(&emoji) {
            return Err(ReactionError::CustomEmojiShortcode(emoji));
        }
        Ok(Self::Emoji(emoji))
    }

    /// The exact `content` this reaction renders as.
    pub fn content(&self) -> &str {
        match self {
            Self::Like => "+",
            Self::Dislike => "-",
            Self::Emoji(emoji) => emoji,
        }
    }
}

/// Why a reaction's content was refused. Exhaustive; every variant is
/// constructed by [`Reaction::emoji`] and covered by a test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReactionError {
    /// The emoji was the empty string, which NIP-25 reads as `+`.
    EmptyEmoji,
    /// The emoji was `:shortcode:`-shaped, which needs a NIP-30 `emoji` row
    /// this door does not write.
    CustomEmojiShortcode(String),
}

impl std::fmt::Display for ReactionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyEmoji => f.write_str(
                "an empty reaction is NIP-25's spelling of a like, not a neutral reaction",
            ),
            Self::CustomEmojiShortcode(got) => write!(
                f,
                "{got} is a NIP-30 custom-emoji shortcode, which needs a companion emoji row this door does not write"
            ),
        }
    }
}

impl std::error::Error for ReactionError {}

fn is_shortcode(emoji: &str) -> bool {
    emoji.len() > 2 && emoji.starts_with(':') && emoji.ends_with(':')
}

/// Compose a NIP-25 reaction to `target`, observed at `sources`.
///
/// The rows come from the one tagging door's ENTITY form, not its threading
/// one, and that is not an optimisation: NIP-25 says *"There MUST be always an
/// `e` tag set to the `id` of the event that is being reacted to"*, and
/// threading a reaction to a reply emits the thread ROOT's `e` row first, so a
/// client tallying reactions by the first `e` would credit the root with a
/// reaction nobody gave it. It is the same reason NIP-18's repost names the
/// entity.
///
/// The carried mentions are declined, which is per-relationship and stated
/// here rather than assumed: a reply carries the parent's `p` rows forward
/// because NIP-10 says to, and a reaction does not because NIP-25 says *"If a
/// client decides to include other `p` tags, which not recommended…"*. What
/// survives is the ONE `p` row naming the author being reacted to, which is
/// the row NIP-25 asks for.
pub fn react(target: &Event, sources: Option<RelayUrl>, reaction: Reaction) -> EventBuilder {
    let mut builder = EventBuilder::new(Kind::from(REACTION_KIND)).content(reaction.content());
    for row in entity_rows(
        target,
        sources,
        &TagOptions::default().without_carried_mentions(),
    ) {
        builder = builder.tag(row);
    }
    builder
}

