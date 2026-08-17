//! Optional, parser-only Nostr content semantics.
//!
//! This crate owns only source text -> semantic document parsing. Parsed
//! references carry the exact engine-free [`nmp_grammar::NostrEntity`] locator
//! value, but this crate does not decide whether to resolve it. A bare `npub`
//! remains distinct from `nprofile`; no event kind, demand, routing,
//! relay admission, or observation policy is inferred here. This crate owns no
//! protocol schema/codec, renderer, component registry, query handle, cache,
//! engine, or network client.

#![deny(unsafe_code)]

mod document;
mod parse;

pub use document::{
    BlockKind, ContentBlock, ContentDiagnostic, ContentDocument, ContentSyntax, InlineNode,
    InlineStyle, ReferenceOccurrence, ReferencePlacement, SourceRange,
};
pub use parse::{parse_content, MAX_CONTENT_BYTES};
