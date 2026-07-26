//! `nmp-nip22` -- the opt-in NIP-22 typed-comments-over-NIP-73-targets
//! protocol crate (#572), on the `nmp-nip68`/`nmp-nip29` template: zero
//! core/engine/store changes.
//! Core stays content-agnostic; this module owns kind:1111's exact
//! schema, semantic root/parent validation, immutable draft construction,
//! and root-thread demand -- never a UI, ranking, moderation, or product
//! policy.
//!
//! Same discipline as `nmp-nip29`/`nmp-nip68`/`nmp-blossom`: this crate
//! NEVER signs (`build`/`intent` emit an [`nostr::UnsignedEvent`]/
//! `WriteIntent` for the caller's own signer machinery) and NEVER touches
//! the engine -- `author`/`created_at` are explicit caller-supplied
//! parameters (this issue's own design decision), so there is no
//! `active_account()` query or wall-clock read anywhere in this crate, and
//! therefore no `engine` feature at all (unlike `nmp-nip29`, which needs
//! one for its semantic kind:9 operation).

mod build;
mod decode;
mod demand;
mod intent;
mod root;
mod target;

pub use build::{compose_comment, compose_comment_reply, compose_top_level_comment};
pub use decode::{decode_comment, CommentDecodeError, DecodedComment};
pub use demand::comment_thread_demand;
pub use intent::comment_intent;
pub use root::{CommentParent, CommentRoot, COMMENT_KIND};
pub use target::{Nip73Target, Nip73TargetError};
