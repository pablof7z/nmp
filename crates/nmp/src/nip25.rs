//! NIP-25 reactions (kind:7) projected through the canonical facade (#155).
//!
//! `nmp-nip25` owns the kind, the three content readings NIP-25 itself defines,
//! and the entity a reaction points at. This module re-exports that vocabulary
//! so the ONE supported product surface owns it for every consumer: a
//! direct-Rust app and `nmp-ffi` resolve the same [`Reaction`] values and the
//! same [`react`] composer, instead of each binding the mechanism crate
//! independently.
//!
//! It is wired here at birth rather than added to the list in #1239, which
//! records four protocol families `nmp-ffi` reaches and this facade does not --
//! a split that penalises the tier NMP calls primary, because a Swift app links
//! one staticlib and gets all of them while a direct-Rust app needs a second
//! Cargo dependency.
//!
//! The crate is engine-free by construction: [`react`] returns an
//! [`crate::EventBuilder`], names no author and reads no clock, so the engine
//! resolves the identity and stamps the time at acceptance exactly as it does
//! for every other write.

pub use nmp_nip25::{react, Reaction, ReactionError, REACTION_KIND};
