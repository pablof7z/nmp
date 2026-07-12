//! Optional, UI-blind Nostr content semantics.
//!
//! This crate owns pure parsing, public-reference normalization, kind:0 and
//! NIP-23 decoding, and the ordinary-NMP demand plan for a reference. It owns
//! no renderer, component registry, query handle, cache, or network client.

#![deny(unsafe_code)]

mod article;
mod document;
mod hydration;
mod parse;
mod profile;
mod reference;

pub use article::{decode_article, decode_article_from_raw, Article};
pub use document::{
    BlockKind, ContentBlock, ContentDiagnostic, ContentDocument, ContentSyntax, InlineNode,
    InlineStyle, ReferenceOccurrence, ReferencePlacement, SourceRange,
};
pub use hydration::{
    evaluate_claim, evaluate_resolution, ClaimDecision, HydrationPolicy, ResolutionDecision,
};
pub use parse::{parse_content, MAX_CONTENT_BYTES};
pub use profile::{decode_profile, decode_profile_from_raw, ProfileMetadata};
pub use reference::{ReferenceDemandPlan, ReferenceTarget};
