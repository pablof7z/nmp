//! Optional, parser-only Nostr content semantics.
//!
//! This crate owns only source text -> semantic document parsing. Parsed
//! references carry the engine-free [`nmp_grammar::reference::ReferenceTarget`]
//! value, but this crate does not decide whether to resolve it. It owns no
//! protocol schema/codec, demand plan, renderer, component registry, query
//! handle, cache, engine, or network client.

#![deny(unsafe_code)]

mod document;
mod parse;

pub use document::{
    BlockKind, ContentBlock, ContentDiagnostic, ContentDocument, ContentSyntax, InlineNode,
    InlineStyle, ReferenceOccurrence, ReferencePlacement, SourceRange,
};
pub use parse::{parse_content, MAX_CONTENT_BYTES};
