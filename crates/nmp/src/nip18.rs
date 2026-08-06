//! NIP-18 reposts (kind:6 and kind:16) projected through the canonical facade
//! (#1239).
//!
//! `nmp-nip18` owns both kinds NIP-18 defines and the two-way split between
//! them, so a caller says "repost this" once and never picks a kind. That is
//! the whole value of the door, and it is exactly what a caller who cannot
//! reach it loses: the alternative to [`repost`] is writing
//! `EventBuilder::new(Kind::from(6))` by hand and getting the kind:16 case
//! wrong. Leaving that door open only to Swift meant the hand-written kind was
//! the direct-Rust default.
//!
//! It returns a [`crate::EventBuilder`], names no author and reads no clock,
//! so the engine resolves the identity and stamps the time at acceptance
//! exactly as it does for every other write.

pub use nmp_nip18::{repost, GENERIC_REPOST_KIND, REPOST_KIND};
