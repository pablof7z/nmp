use crate::ReferenceTarget;

/// Input syntax selected by the caller or an owning protocol module.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ContentSyntax {
    /// Plain Nostr content with NIP-21/NIP-27 references.
    #[default]
    PlainText,
    /// CommonMark-compatible Markdown, including Nostr references inside
    /// ordinary inline text.
    Markdown,
}

/// A half-open UTF-8 byte range into the exact original content.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceRange {
    pub start: u32,
    pub end: u32,
}

impl SourceRange {
    pub(crate) fn from_usize(start: usize, end: usize) -> Self {
        Self {
            start: start as u32,
            end: end as u32,
        }
    }
}

/// Semantic block context. These are document facts, not public UI component
/// names; a platform renderer may handle them cohesively inside one content
/// view.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BlockKind {
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

/// Inline author syntax that survives into native rendering.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum InlineStyle {
    Emphasis,
    Strong,
    Strikethrough,
    Code,
}

/// Where a reference occurred in the authored document.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReferencePlacement {
    Inline,
    Standalone,
}

/// One occurrence of a normalized Nostr reference.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReferenceOccurrence {
    pub id: u64,
    pub original: String,
    pub target: ReferenceTarget,
    pub source: SourceRange,
    pub placement: ReferencePlacement,
}

/// One inline semantic node. Every variant carries its original source range;
/// dynamic rendering never mutates canonical source identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InlineNode {
    Text {
        text: String,
        source: SourceRange,
        styles: Vec<InlineStyle>,
    },
    Reference {
        occurrence: ReferenceOccurrence,
        styles: Vec<InlineStyle>,
    },
    Hashtag {
        hashtag: String,
        original: String,
        source: SourceRange,
        styles: Vec<InlineStyle>,
    },
    Link {
        destination: String,
        label: String,
        source: SourceRange,
        styles: Vec<InlineStyle>,
    },
    SoftBreak {
        source: SourceRange,
    },
    HardBreak {
        source: SourceRange,
    },
}

impl InlineNode {
    pub(crate) fn source(&self) -> SourceRange {
        match self {
            Self::Text { source, .. }
            | Self::Hashtag { source, .. }
            | Self::Link { source, .. }
            | Self::SoftBreak { source }
            | Self::HardBreak { source } => *source,
            Self::Reference { occurrence, .. } => occurrence.source,
        }
    }

    pub(crate) fn is_visible_non_whitespace(&self) -> bool {
        match self {
            Self::Text { text, .. } => !text.trim().is_empty(),
            Self::SoftBreak { .. } | Self::HardBreak { .. } => false,
            _ => true,
        }
    }
}

/// One document block with stable source identity and flat inline children.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContentBlock {
    pub id: u64,
    pub kind: BlockKind,
    pub source: SourceRange,
    pub inlines: Vec<InlineNode>,
}

impl ContentBlock {
    pub(crate) fn finalize_reference_placement(&mut self) {
        let only_reference = self
            .inlines
            .iter()
            .filter(|node| node.is_visible_non_whitespace())
            .count()
            == 1;
        if !only_reference {
            return;
        }
        for node in &mut self.inlines {
            if let InlineNode::Reference { occurrence, .. } = node {
                occurrence.placement = ReferencePlacement::Standalone;
            }
        }
    }
}

/// Honest parse diagnostics. Unsupported input stays visible through text
/// fallback and diagnostics explain why richer semantics were not emitted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContentDiagnostic {
    InputTruncated {
        original_bytes: u64,
        parsed_bytes: u64,
    },
    MalformedReference {
        original: String,
        source: SourceRange,
    },
}

/// Pure parsing result. It contains no resolution or presentation state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContentDocument {
    pub syntax: ContentSyntax,
    pub blocks: Vec<ContentBlock>,
    pub diagnostics: Vec<ContentDiagnostic>,
}

impl ContentDocument {
    /// Every authored occurrence, preserving duplicate mentions of one target.
    pub fn references(&self) -> Vec<&ReferenceOccurrence> {
        self.blocks
            .iter()
            .flat_map(|block| block.inlines.iter())
            .filter_map(|node| match node {
                InlineNode::Reference { occurrence, .. } => Some(occurrence),
                _ => None,
            })
            .collect()
    }
}

pub(crate) fn stable_id(range: SourceRange, discriminator: u8) -> u64 {
    ((range.start as u64) << 32) ^ ((range.end as u64) << 8) ^ u64::from(discriminator)
}
