//! UniFFI projection of the optional, UI-blind content semantic layer.

use nmp_content::{
    BlockKind, ClaimDecision, ContentDiagnostic, ContentSyntax, HydrationPolicy, InlineNode,
    InlineStyle, ReferencePlacement, ReferenceTarget, ResolutionDecision, SourceRange,
};
use uniffi::{Enum, Record};

use crate::convert::demand_to_ffi;
use crate::types::{FfiDemand, FfiRow};

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

#[derive(Debug, Clone, PartialEq, Eq, Enum)]
pub enum FfiReferenceTarget {
    Profile {
        pubkey: String,
        relay_hints: Vec<String>,
    },
    Event {
        id: String,
        author_hint: Option<String>,
        kind_hint: Option<u16>,
        relay_hints: Vec<String>,
    },
    Address {
        kind: u16,
        author: String,
        identifier: String,
        relay_hints: Vec<String>,
    },
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

#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiProfileMetadata {
    pub pubkey: String,
    pub name: Option<String>,
    pub display_name: Option<String>,
    pub about: Option<String>,
    pub picture: Option<String>,
    pub banner: Option<String>,
    pub nip05: Option<String>,
    pub lud06: Option<String>,
    pub lud16: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiArticle {
    pub event_id: String,
    pub author: String,
    pub created_at: u64,
    pub identifier: String,
    pub title: Option<String>,
    pub summary: Option<String>,
    pub image: Option<String>,
    pub published_at: Option<u64>,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Record)]
pub struct FfiReferenceDemandPlan {
    pub target_key: String,
    pub canonical: FfiDemand,
    pub helpers: Vec<FfiDemand>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Record)]
pub struct FfiContentHydrationPolicy {
    pub max_active_references: u32,
    pub max_resolved_references: u32,
    pub max_depth: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Enum)]
pub enum FfiContentClaimDecision {
    Acquire,
    Cycle { target_key: String },
    DepthLimit { maximum: u8 },
    ActiveLimit { maximum: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Enum)]
pub enum FfiContentResolutionDecision {
    Accept,
    ResolvedLimit { maximum: u32 },
}

#[uniffi::export]
pub fn parse_nostr_content(content: String, syntax: FfiContentSyntax) -> FfiContentDocument {
    document_to_ffi(nmp_content::parse_content(
        &content,
        syntax_from_ffi(syntax),
    ))
}

#[uniffi::export]
pub fn decode_profile_resource(row: FfiRow) -> FfiProfileMetadata {
    let profile = nmp_content::decode_profile_from_raw(&row.pubkey, &row.content);
    FfiProfileMetadata {
        pubkey: profile.pubkey,
        name: profile.name,
        display_name: profile.display_name,
        about: profile.about,
        picture: profile.picture,
        banner: profile.banner,
        nip05: profile.nip05,
        lud06: profile.lud06,
        lud16: profile.lud16,
    }
}

#[uniffi::export]
pub fn decode_article_resource(row: FfiRow) -> FfiArticle {
    let article = nmp_content::decode_article_from_raw(
        &row.id,
        &row.pubkey,
        row.created_at,
        row.tags.iter().map(Vec::as_slice),
        &row.content,
    );
    FfiArticle {
        event_id: article.event_id,
        author: article.author,
        created_at: article.created_at,
        identifier: article.identifier,
        title: article.title,
        summary: article.summary,
        image: article.image,
        published_at: article.published_at,
        content: article.content,
    }
}

#[uniffi::export]
pub fn content_reference_demand_plan(target: FfiReferenceTarget) -> FfiReferenceDemandPlan {
    let plan = target_from_ffi(target).demand_plan();
    FfiReferenceDemandPlan {
        target_key: plan.target_key,
        canonical: demand_to_ffi(plan.canonical),
        helpers: plan.helpers.into_iter().map(demand_to_ffi).collect(),
    }
}

#[uniffi::export]
pub fn evaluate_content_claim(
    target_key: String,
    path: Vec<String>,
    depth: u8,
    active_references: u32,
    policy: FfiContentHydrationPolicy,
) -> FfiContentClaimDecision {
    match nmp_content::evaluate_claim(
        &target_key,
        &path,
        depth,
        active_references,
        hydration_policy_from_ffi(policy),
    ) {
        ClaimDecision::Acquire => FfiContentClaimDecision::Acquire,
        ClaimDecision::Cycle { target_key } => FfiContentClaimDecision::Cycle { target_key },
        ClaimDecision::DepthLimit { maximum } => FfiContentClaimDecision::DepthLimit { maximum },
        ClaimDecision::ActiveLimit { maximum } => FfiContentClaimDecision::ActiveLimit { maximum },
    }
}

#[uniffi::export]
pub fn evaluate_content_resolution(
    target_already_resolved: bool,
    resolved_references: u32,
    policy: FfiContentHydrationPolicy,
) -> FfiContentResolutionDecision {
    match nmp_content::evaluate_resolution(
        target_already_resolved,
        resolved_references,
        hydration_policy_from_ffi(policy),
    ) {
        ResolutionDecision::Accept => FfiContentResolutionDecision::Accept,
        ResolutionDecision::ResolvedLimit { maximum } => {
            FfiContentResolutionDecision::ResolvedLimit { maximum }
        }
    }
}

fn hydration_policy_from_ffi(value: FfiContentHydrationPolicy) -> HydrationPolicy {
    HydrationPolicy {
        max_active_references: value.max_active_references,
        max_resolved_references: value.max_resolved_references,
        max_depth: value.max_depth,
    }
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

fn target_to_ffi(value: ReferenceTarget) -> FfiReferenceTarget {
    match value {
        ReferenceTarget::Profile {
            pubkey,
            relay_hints,
        } => FfiReferenceTarget::Profile {
            pubkey,
            relay_hints,
        },
        ReferenceTarget::Event {
            id,
            author_hint,
            kind_hint,
            relay_hints,
        } => FfiReferenceTarget::Event {
            id,
            author_hint,
            kind_hint,
            relay_hints,
        },
        ReferenceTarget::Address {
            kind,
            author,
            identifier,
            relay_hints,
        } => FfiReferenceTarget::Address {
            kind,
            author,
            identifier,
            relay_hints,
        },
    }
}

fn target_from_ffi(value: FfiReferenceTarget) -> ReferenceTarget {
    match value {
        FfiReferenceTarget::Profile {
            pubkey,
            relay_hints,
        } => ReferenceTarget::Profile {
            pubkey,
            relay_hints,
        },
        FfiReferenceTarget::Event {
            id,
            author_hint,
            kind_hint,
            relay_hints,
        } => ReferenceTarget::Event {
            id,
            author_hint,
            kind_hint,
            relay_hints,
        },
        FfiReferenceTarget::Address {
            kind,
            author,
            identifier,
            relay_hints,
        } => ReferenceTarget::Address {
            kind,
            author,
            identifier,
            relay_hints,
        },
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

    #[test]
    fn ffi_profile_decode_is_infallible() {
        let profile = decode_profile_resource(FfiRow {
            id: "id".to_string(),
            pubkey: "pk".to_string(),
            created_at: 1,
            kind: 0,
            tags: vec![],
            content: r#"{"display_name":"Alice"}"#.to_string(),
            sig: "sig".to_string(),
            sources: vec![],
        });
        assert_eq!(profile.display_name.as_deref(), Some("Alice"));
    }
}
