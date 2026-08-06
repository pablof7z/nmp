//! Nostr content parsing projected through the canonical facade (#1239).
//!
//! `nmp-content` owns source text -> semantic document parsing and nothing
//! else: no kind, no demand, no relay admission, no decision about whether a
//! reference it found should be resolved. This module re-exports that
//! vocabulary so the ONE supported product surface owns it for every consumer.
//!
//! The consequence of it being missing is on the record. `nmp-ffi` has bound
//! this crate since the content projection landed, so a Swift app renders
//! `nostr:` references through [`parse_content`] by linking one staticlib.
//! mosaico, a direct-Rust app, hand-rolled a `find("nostr:")` scanner with an
//! ASCII-span heuristic and a two-prefix filter instead -- silently dropping
//! every `note1`/`nevent1`/`naddr1` in a body and never noticing an `nsec1`
//! pasted into one. Same problem, two consumers, and only the Swift one could
//! reach the answer NMP already ships.
//!
//! A parsed reference carries [`crate::NostrEntity`], which this facade already
//! re-exports, so naming what the parser returns needs no second crate.

pub use nmp_content::{
    parse_content, BlockKind, ContentBlock, ContentDiagnostic, ContentDocument, ContentSyntax,
    InlineNode, InlineStyle, ReferenceOccurrence, ReferencePlacement, SourceRange,
    MAX_CONTENT_BYTES,
};
