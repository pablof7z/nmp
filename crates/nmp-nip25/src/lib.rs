//! `nmp-nip25` -- typed native-event reactions (kind:7) over NMP's canonical
//! row path (#155).
//!
//! This crate owns the NIP-25 target schema, reaction-value validation, and
//! immutable unsigned draft construction. A target is qualified by an
//! ordinary cache-only NMP query for one exact event id; callers cannot supply
//! an author, kind, coordinate, or relay hint independently. Draft composition
//! freezes the engine's active account and reads the event time in Rust.
//!
//! Deliberately absent: group `h` context, relay routing, signing, durable
//! acceptance, receipt/retry lifecycle, reaction aggregation, or NIP-09
//! deletion. A group publisher may consume [`ReactionDraft::into_event`] and
//! add its own closed context, but this module never mints group authority.

mod draft;
mod target;
mod value;

pub use draft::{reaction_draft, ReactionDraft, ReactionDraftError};
pub use target::{reaction_target, ReactionTarget, ReactionTargetError};
pub use value::{ReactionValue, ReactionValueError};

/// The only event schema claimed by this first NIP-25 module. Kind:17
/// external-content reactions and NIP-09 kind:5 deletion requests are
/// deliberately separate.
pub const REACTION_KIND: u16 = 7;

#[cfg(test)]
mod tests;
