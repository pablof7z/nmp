//! NIP-22 comments (kind:1111 over NIP-73 external targets) projected through
//! the canonical facade (#851).
//!
//! `nmp-nip22` owns comment validation, exact composition, and strict decode.
//! This module re-exports that vocabulary so the ONE supported product surface
//! owns it for every consumer: a direct-Rust app and `nmp-ffi` resolve the same
//! `CommentRoot`/`CommentParent`/`Nip73Target` values and the same
//! [`comment_intent`] write operation, instead of each binding the mechanism
//! crate independently and having to keep two owners aligned by convention.
//!
//! The crate is engine-free by construction -- [`comment_intent`] takes its
//! author and timestamp explicitly and never reads an ambient active account --
//! so nothing here needs the engine to compose a comment. What it returns is an
//! ordinary [`crate::WriteIntent`] (#907), published through the same
//! `Engine::publish` lifecycle as every other write; NIP-22 owns no separate
//! correlation, take-once, signing, routing, receipt, or retry machinery.

pub use nmp_nip22::{
    comment_intent, comment_thread_demand, compose_comment_reply, compose_top_level_comment,
    decode_comment, CommentDecodeError, CommentParent, CommentRoot, DecodedComment, Nip73Target,
    Nip73TargetError, COMMENT_KIND,
};
