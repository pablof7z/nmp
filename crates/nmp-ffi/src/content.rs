//! UniFFI projection of the optional, UI-blind content semantic layer.

use nmp_content::{
    BlockKind, ContentDiagnostic, ContentSyntax, InlineNode, InlineStyle, ReferencePlacement,
    SourceRange,
};
use uniffi::{Enum, Record};

use crate::reference::{target_to_ffi, FfiReferenceTarget};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub enum FfiContentSyntax {
    PlainText,
    Markdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Record)]
pub struct FfiSourceRange {
    pub start: u32,
    pub end: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Enum)]
pub enum FfiBlockKind {
    Paragraph,
    Heading {
        level: u8,
    },
    Quote {
        depth: u8,
    },
    ListItem {
        ordered: bool,
        ordinal: Option<u64>,
        depth: u8,
    },
    Code {
        language: Option<String>,
    },
    ThematicBreak,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub enum FfiInlineStyle {
    Emphasis,
    Strong,
    Strikethrough,
    Code,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub enum FfiReferencePlacement {
    Inline,
    Standalone,
}

#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiReferenceOccurrence {
    pub id: u64,
    pub original: String,
    pub target: FfiReferenceTarget,
    pub source: FfiSourceRange,
    pub placement: FfiReferencePlacement,
}

#[derive(Debug, Clone, PartialEq, Eq, Enum)]
pub enum FfiInlineNode {
    Text {
        text: String,
        source: FfiSourceRange,
        styles: Vec<FfiInlineStyle>,
    },
    Reference {
        occurrence: FfiReferenceOccurrence,
        styles: Vec<FfiInlineStyle>,
    },
    Hashtag {
        hashtag: String,
        original: String,
        source: FfiSourceRange,
        styles: Vec<FfiInlineStyle>,
    },
    Link {
        destination: String,
        label: String,
        source: FfiSourceRange,
        styles: Vec<FfiInlineStyle>,
    },
    SoftBreak {
        source: FfiSourceRange,
    },
    HardBreak {
        source: FfiSourceRange,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiContentBlock {
    pub id: u64,
    pub kind: FfiBlockKind,
    pub source: FfiSourceRange,
    pub inlines: Vec<FfiInlineNode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Enum)]
pub enum FfiContentDiagnostic {
    InputTruncated {
        original_bytes: u64,
        parsed_bytes: u64,
    },
    MalformedReference {
        original: String,
        source: FfiSourceRange,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiContentDocument {
    pub syntax: FfiContentSyntax,
    pub blocks: Vec<FfiContentBlock>,
    pub diagnostics: Vec<FfiContentDiagnostic>,
}

#[uniffi::export]
pub fn parse_nostr_content(content: String, syntax: FfiContentSyntax) -> FfiContentDocument {
    document_to_ffi(nmp_content::parse_content(
        &content,
        syntax_from_ffi(syntax),
    ))
}

fn syntax_from_ffi(value: FfiContentSyntax) -> ContentSyntax {
    match value {
        FfiContentSyntax::PlainText => ContentSyntax::PlainText,
        FfiContentSyntax::Markdown => ContentSyntax::Markdown,
    }
}

fn syntax_to_ffi(value: ContentSyntax) -> FfiContentSyntax {
    match value {
        ContentSyntax::PlainText => FfiContentSyntax::PlainText,
        ContentSyntax::Markdown => FfiContentSyntax::Markdown,
    }
}

fn range_to_ffi(value: SourceRange) -> FfiSourceRange {
    FfiSourceRange {
        start: value.start,
        end: value.end,
    }
}

fn block_kind_to_ffi(value: BlockKind) -> FfiBlockKind {
    match value {
        BlockKind::Paragraph => FfiBlockKind::Paragraph,
        BlockKind::Heading { level } => FfiBlockKind::Heading { level },
        BlockKind::Quote { depth } => FfiBlockKind::Quote { depth },
        BlockKind::ListItem {
            ordered,
            ordinal,
            depth,
        } => FfiBlockKind::ListItem {
            ordered,
            ordinal,
            depth,
        },
        BlockKind::Code { language } => FfiBlockKind::Code { language },
        BlockKind::ThematicBreak => FfiBlockKind::ThematicBreak,
    }
}

fn style_to_ffi(value: InlineStyle) -> FfiInlineStyle {
    match value {
        InlineStyle::Emphasis => FfiInlineStyle::Emphasis,
        InlineStyle::Strong => FfiInlineStyle::Strong,
        InlineStyle::Strikethrough => FfiInlineStyle::Strikethrough,
        InlineStyle::Code => FfiInlineStyle::Code,
    }
}

fn placement_to_ffi(value: ReferencePlacement) -> FfiReferencePlacement {
    match value {
        ReferencePlacement::Inline => FfiReferencePlacement::Inline,
        ReferencePlacement::Standalone => FfiReferencePlacement::Standalone,
    }
}

fn occurrence_to_ffi(value: nmp_content::ReferenceOccurrence) -> FfiReferenceOccurrence {
    FfiReferenceOccurrence {
        id: value.id,
        original: value.original,
        target: target_to_ffi(value.target),
        source: range_to_ffi(value.source),
        placement: placement_to_ffi(value.placement),
    }
}

fn node_to_ffi(value: InlineNode) -> FfiInlineNode {
    match value {
        InlineNode::Text {
            text,
            source,
            styles,
        } => FfiInlineNode::Text {
            text,
            source: range_to_ffi(source),
            styles: styles.into_iter().map(style_to_ffi).collect(),
        },
        InlineNode::Reference { occurrence, styles } => FfiInlineNode::Reference {
            occurrence: occurrence_to_ffi(occurrence),
            styles: styles.into_iter().map(style_to_ffi).collect(),
        },
        InlineNode::Hashtag {
            hashtag,
            original,
            source,
            styles,
        } => FfiInlineNode::Hashtag {
            hashtag,
            original,
            source: range_to_ffi(source),
            styles: styles.into_iter().map(style_to_ffi).collect(),
        },
        InlineNode::Link {
            destination,
            label,
            source,
            styles,
        } => FfiInlineNode::Link {
            destination,
            label,
            source: range_to_ffi(source),
            styles: styles.into_iter().map(style_to_ffi).collect(),
        },
        InlineNode::SoftBreak { source } => FfiInlineNode::SoftBreak {
            source: range_to_ffi(source),
        },
        InlineNode::HardBreak { source } => FfiInlineNode::HardBreak {
            source: range_to_ffi(source),
        },
    }
}

fn diagnostic_to_ffi(value: ContentDiagnostic) -> FfiContentDiagnostic {
    match value {
        ContentDiagnostic::InputTruncated {
            original_bytes,
            parsed_bytes,
        } => FfiContentDiagnostic::InputTruncated {
            original_bytes,
            parsed_bytes,
        },
        ContentDiagnostic::MalformedReference { original, source } => {
            FfiContentDiagnostic::MalformedReference {
                original,
                source: range_to_ffi(source),
            }
        }
    }
}

fn document_to_ffi(value: nmp_content::ContentDocument) -> FfiContentDocument {
    FfiContentDocument {
        syntax: syntax_to_ffi(value.syntax),
        blocks: value
            .blocks
            .into_iter()
            .map(|block| FfiContentBlock {
                id: block.id,
                kind: block_kind_to_ffi(block.kind),
                source: range_to_ffi(block.source),
                inlines: block.inlines.into_iter().map(node_to_ffi).collect(),
            })
            .collect(),
        diagnostics: value
            .diagnostics
            .into_iter()
            .map(diagnostic_to_ffi)
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffi_document_keeps_reference_and_source_range() {
        let source = "hello npub14f8usejl26twx0dhuxjh9cas7keav9vr0v8nvtwtrjqx3vycc76qqh9nsy";
        let document = parse_nostr_content(source.to_string(), FfiContentSyntax::PlainText);
        assert_eq!(document.blocks.len(), 1);
        assert!(document.blocks[0]
            .inlines
            .iter()
            .any(|node| matches!(node, FfiInlineNode::Reference { .. })));
    }
}
