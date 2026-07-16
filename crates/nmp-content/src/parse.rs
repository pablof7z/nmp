use std::ops::Range;
use std::sync::OnceLock;

use nmp_grammar::{decode_nostr_entity, reference::ReferenceTarget};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use regex::Regex;

use crate::document::{stable_id, ContentDiagnostic};
use crate::{
    BlockKind, ContentBlock, ContentDocument, ContentSyntax, InlineNode, InlineStyle,
    ReferenceOccurrence, ReferencePlacement, SourceRange,
};

/// Hard bound for one parse projection crossing a native boundary.
pub const MAX_CONTENT_BYTES: usize = 256 * 1024;

/// Parse readable Nostr content without performing any I/O.
#[must_use]
pub fn parse_content(content: &str, syntax: ContentSyntax) -> ContentDocument {
    let (input, mut diagnostics) = bounded_input(content);
    let mut blocks = match syntax {
        ContentSyntax::PlainText => parse_plain(input, &mut diagnostics),
        ContentSyntax::Markdown => parse_markdown(input, &mut diagnostics),
    };
    for block in &mut blocks {
        block.finalize_reference_placement();
    }
    ContentDocument {
        syntax,
        blocks,
        diagnostics,
    }
}

fn bounded_input(content: &str) -> (&str, Vec<ContentDiagnostic>) {
    if content.len() <= MAX_CONTENT_BYTES {
        return (content, Vec::new());
    }
    let mut end = MAX_CONTENT_BYTES;
    while !content.is_char_boundary(end) {
        end -= 1;
    }
    (
        &content[..end],
        vec![ContentDiagnostic::InputTruncated {
            original_bytes: content.len() as u64,
            parsed_bytes: end as u64,
        }],
    )
}

fn parse_plain(content: &str, diagnostics: &mut Vec<ContentDiagnostic>) -> Vec<ContentBlock> {
    let mut blocks = Vec::new();
    let mut start = 0usize;
    while start < content.len() {
        while start < content.len() && content.as_bytes()[start] == b'\n' {
            start += 1;
        }
        if start >= content.len() {
            break;
        }

        let end = find_paragraph_end(content, start);
        let source = SourceRange::from_usize(start, end);
        let mut inlines = tokenize_inline(&content[start..end], start, &[], diagnostics);
        split_text_line_breaks(&mut inlines);
        blocks.push(ContentBlock {
            id: stable_id(source, 1),
            kind: BlockKind::Paragraph,
            source,
            inlines,
        });
        start = end;
    }
    blocks
}

fn find_paragraph_end(content: &str, start: usize) -> usize {
    let bytes = content.as_bytes();
    let mut index = start;
    while index + 1 < bytes.len() {
        if bytes[index] == b'\n' && bytes[index + 1] == b'\n' {
            return index;
        }
        index += 1;
    }
    content.len()
}

fn parse_markdown(content: &str, diagnostics: &mut Vec<ContentDiagnostic>) -> Vec<ContentBlock> {
    let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let parser = Parser::new_ext(content, options).into_offset_iter();
    let mut state = MarkdownState::default();

    for (event, range) in parser {
        state.consume(content, event, range, diagnostics);
    }
    state.flush();
    state.blocks
}

#[derive(Default)]
struct MarkdownState {
    blocks: Vec<ContentBlock>,
    current: Option<BlockBuilder>,
    styles: Vec<InlineStyle>,
    active_link: Option<String>,
    quote_depth: u8,
    lists: Vec<ListState>,
    in_item: bool,
    code_language: Option<String>,
}

#[derive(Clone, Copy)]
struct ListState {
    ordered: bool,
    next: Option<u64>,
}

struct BlockBuilder {
    kind: BlockKind,
    inlines: Vec<InlineNode>,
    start: usize,
    end: usize,
}

impl MarkdownState {
    fn consume(
        &mut self,
        content: &str,
        event: Event<'_>,
        range: Range<usize>,
        diagnostics: &mut Vec<ContentDiagnostic>,
    ) {
        match event {
            Event::Start(tag) => self.start(tag, range.start),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => {
                self.ensure_text_block(range.start);
                if self.code_language.is_some() {
                    self.push(InlineNode::Text {
                        text: text.into_string(),
                        source: SourceRange::from_usize(range.start, range.end),
                        styles: vec![InlineStyle::Code],
                    });
                } else if let Some(destination) = &self.active_link {
                    self.push(InlineNode::Link {
                        destination: destination.clone(),
                        label: text.into_string(),
                        source: SourceRange::from_usize(range.start, range.end),
                        styles: normalized_styles(&self.styles),
                    });
                } else {
                    let nodes =
                        tokenize_inline(text.as_ref(), range.start, &self.styles, diagnostics);
                    for node in nodes {
                        self.push(node);
                    }
                }
            }
            Event::Code(code) => {
                self.ensure_text_block(range.start);
                let mut styles = self.styles.clone();
                styles.push(InlineStyle::Code);
                self.push(InlineNode::Text {
                    text: code.into_string(),
                    source: SourceRange::from_usize(range.start, range.end),
                    styles: normalized_styles(&styles),
                });
            }
            Event::SoftBreak => {
                self.ensure_text_block(range.start);
                self.push(InlineNode::SoftBreak {
                    source: SourceRange::from_usize(range.start, range.end),
                });
            }
            Event::HardBreak => {
                self.ensure_text_block(range.start);
                self.push(InlineNode::HardBreak {
                    source: SourceRange::from_usize(range.start, range.end),
                });
            }
            Event::Rule => {
                self.flush();
                let source = SourceRange::from_usize(range.start, range.end);
                self.blocks.push(ContentBlock {
                    id: stable_id(source, 6),
                    kind: BlockKind::ThematicBreak,
                    source,
                    inlines: Vec::new(),
                });
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                self.ensure_text_block(range.start);
                self.push(InlineNode::Text {
                    text: html.into_string(),
                    source: SourceRange::from_usize(range.start, range.end),
                    styles: normalized_styles(&self.styles),
                });
            }
            Event::FootnoteReference(label) => {
                self.ensure_text_block(range.start);
                self.push(InlineNode::Text {
                    text: format!("[^{label}]"),
                    source: SourceRange::from_usize(range.start, range.end),
                    styles: normalized_styles(&self.styles),
                });
            }
            Event::TaskListMarker(checked) => {
                self.ensure_text_block(range.start);
                self.push(InlineNode::Text {
                    text: if checked { "☑ " } else { "☐ " }.to_string(),
                    source: SourceRange::from_usize(range.start, range.end),
                    styles: normalized_styles(&self.styles),
                });
            }
            Event::InlineMath(value) | Event::DisplayMath(value) => {
                self.ensure_text_block(range.start);
                self.push(InlineNode::Text {
                    text: value.into_string(),
                    source: SourceRange::from_usize(range.start, range.end),
                    styles: normalized_styles(&self.styles),
                });
            }
        }

        if let Some(current) = &mut self.current {
            current.end = current.end.max(range.end.min(content.len()));
        }
    }

    fn start(&mut self, tag: Tag<'_>, start: usize) {
        match tag {
            Tag::Paragraph => self.begin(self.contextual_kind(), start),
            Tag::Heading { level, .. } => {
                self.flush();
                self.begin(
                    BlockKind::Heading {
                        level: heading(level),
                    },
                    start,
                );
            }
            Tag::BlockQuote(_) => self.quote_depth = self.quote_depth.saturating_add(1),
            Tag::CodeBlock(kind) => {
                self.flush();
                let language = match kind {
                    CodeBlockKind::Indented => None,
                    CodeBlockKind::Fenced(value) => {
                        let value = value.trim();
                        (!value.is_empty()).then(|| value.to_string())
                    }
                };
                self.code_language = Some(language.clone().unwrap_or_default());
                self.begin(BlockKind::Code { language }, start);
            }
            Tag::List(start) => self.lists.push(ListState {
                ordered: start.is_some(),
                next: start,
            }),
            Tag::Item => {
                self.flush();
                self.in_item = true;
                self.begin(self.contextual_kind(), start);
            }
            Tag::Emphasis => self.styles.push(InlineStyle::Emphasis),
            Tag::Strong => self.styles.push(InlineStyle::Strong),
            Tag::Strikethrough => self.styles.push(InlineStyle::Strikethrough),
            Tag::Link { dest_url, .. } => self.active_link = Some(dest_url.into_string()),
            Tag::Image { dest_url, .. } => self.active_link = Some(dest_url.into_string()),
            Tag::HtmlBlock
            | Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Table(_)
            | Tag::TableHead
            | Tag::TableRow
            | Tag::TableCell
            | Tag::Superscript
            | Tag::Subscript
            | Tag::MetadataBlock(_) => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::Heading(_) => self.flush(),
            TagEnd::BlockQuote(_) => self.quote_depth = self.quote_depth.saturating_sub(1),
            TagEnd::CodeBlock => {
                self.flush();
                self.code_language = None;
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
            }
            TagEnd::Item => {
                self.flush();
                self.in_item = false;
                if let Some(list) = self.lists.last_mut() {
                    if let Some(next) = &mut list.next {
                        *next += 1;
                    }
                }
            }
            TagEnd::Emphasis => remove_style(&mut self.styles, InlineStyle::Emphasis),
            TagEnd::Strong => remove_style(&mut self.styles, InlineStyle::Strong),
            TagEnd::Strikethrough => remove_style(&mut self.styles, InlineStyle::Strikethrough),
            TagEnd::Link | TagEnd::Image => self.active_link = None,
            TagEnd::HtmlBlock
            | TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::Table
            | TagEnd::TableHead
            | TagEnd::TableRow
            | TagEnd::TableCell
            | TagEnd::Superscript
            | TagEnd::Subscript
            | TagEnd::MetadataBlock(_) => {}
        }
    }

    fn contextual_kind(&self) -> BlockKind {
        if self.code_language.is_some() {
            return BlockKind::Code {
                language: self
                    .code_language
                    .as_ref()
                    .filter(|value| !value.is_empty())
                    .cloned(),
            };
        }
        if self.in_item {
            let list = self.lists.last().copied().unwrap_or(ListState {
                ordered: false,
                next: None,
            });
            return BlockKind::ListItem {
                ordered: list.ordered,
                ordinal: list.next,
                depth: self.lists.len().min(u8::MAX as usize) as u8,
            };
        }
        if self.quote_depth > 0 {
            return BlockKind::Quote {
                depth: self.quote_depth,
            };
        }
        BlockKind::Paragraph
    }

    fn ensure_text_block(&mut self, start: usize) {
        if self.current.is_none() {
            self.begin(self.contextual_kind(), start);
        }
    }

    fn begin(&mut self, kind: BlockKind, start: usize) {
        if self.current.is_none() {
            self.current = Some(BlockBuilder {
                kind,
                inlines: Vec::new(),
                start,
                end: start,
            });
        }
    }

    fn push(&mut self, node: InlineNode) {
        if let Some(current) = &mut self.current {
            current.end = current.end.max(node.source().end as usize);
            current.inlines.push(node);
        }
    }

    fn flush(&mut self) {
        let Some(builder) = self.current.take() else {
            return;
        };
        if builder.inlines.is_empty() && !matches!(builder.kind, BlockKind::ThematicBreak) {
            return;
        }
        let source = SourceRange::from_usize(builder.start, builder.end.max(builder.start));
        let discriminator = match builder.kind {
            BlockKind::Paragraph => 1,
            BlockKind::Heading { .. } => 2,
            BlockKind::Quote { .. } => 3,
            BlockKind::ListItem { .. } => 4,
            BlockKind::Code { .. } => 5,
            BlockKind::ThematicBreak => 6,
        };
        self.blocks.push(ContentBlock {
            id: stable_id(source, discriminator),
            kind: builder.kind,
            source,
            inlines: builder.inlines,
        });
    }
}

fn heading(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn remove_style(styles: &mut Vec<InlineStyle>, style: InlineStyle) {
    if let Some(index) = styles.iter().rposition(|candidate| *candidate == style) {
        styles.remove(index);
    }
}

fn normalized_styles(styles: &[InlineStyle]) -> Vec<InlineStyle> {
    let mut result = styles.to_vec();
    result.sort();
    result.dedup();
    result
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TokenKind {
    Reference,
    Url,
    Hashtag,
}

#[derive(Clone, Debug)]
struct TokenMatch {
    start: usize,
    end: usize,
    kind: TokenKind,
}

fn tokenize_inline(
    text: &str,
    base: usize,
    styles: &[InlineStyle],
    diagnostics: &mut Vec<ContentDiagnostic>,
) -> Vec<InlineNode> {
    let mut matches = collect_matches(text);
    matches.sort_by_key(|item| (item.start, token_priority(item.kind), usize::MAX - item.end));

    let mut accepted = Vec::new();
    let mut cursor = 0usize;
    for item in matches {
        if item.start < cursor {
            continue;
        }
        cursor = item.end;
        accepted.push(item);
    }

    let styles = normalized_styles(styles);
    let mut nodes = Vec::new();
    let mut cursor = 0usize;
    for item in accepted {
        if item.start > cursor {
            push_text(&mut nodes, text, cursor..item.start, base, &styles);
        }
        let original = &text[item.start..item.end];
        let source = SourceRange::from_usize(base + item.start, base + item.end);
        match item.kind {
            TokenKind::Reference => match decode_nostr_entity(original) {
                Ok(entity) => nodes.push(InlineNode::Reference {
                    occurrence: ReferenceOccurrence {
                        id: stable_id(source, 20),
                        original: original.to_string(),
                        target: ReferenceTarget::from_entity(entity),
                        source,
                        placement: ReferencePlacement::Inline,
                    },
                    styles: styles.clone(),
                }),
                Err(_) => {
                    diagnostics.push(ContentDiagnostic::MalformedReference {
                        original: original.to_string(),
                        source,
                    });
                    push_text(&mut nodes, text, item.start..item.end, base, &styles);
                }
            },
            TokenKind::Url => nodes.push(InlineNode::Link {
                destination: original.to_string(),
                label: original.to_string(),
                source,
                styles: styles.clone(),
            }),
            TokenKind::Hashtag => nodes.push(InlineNode::Hashtag {
                hashtag: original.trim_start_matches('#').to_lowercase(),
                original: original.to_string(),
                source,
                styles: styles.clone(),
            }),
        }
        cursor = item.end;
    }
    if cursor < text.len() {
        push_text(&mut nodes, text, cursor..text.len(), base, &styles);
    }
    nodes
}

fn collect_matches(text: &str) -> Vec<TokenMatch> {
    let mut matches = Vec::new();
    for found in reference_regex().find_iter(text) {
        matches.push(TokenMatch {
            start: found.start(),
            end: found.end(),
            kind: TokenKind::Reference,
        });
    }
    for found in url_regex().find_iter(text) {
        let mut end = found.end();
        while end > found.start()
            && matches!(
                text.as_bytes()[end - 1],
                b'.' | b',' | b'!' | b'?' | b';' | b':' | b')' | b']' | b'}'
            )
        {
            end -= 1;
        }
        if end > found.start() {
            matches.push(TokenMatch {
                start: found.start(),
                end,
                kind: TokenKind::Url,
            });
        }
    }
    for captures in hashtag_regex().captures_iter(text) {
        let Some(found) = captures.name("tag") else {
            continue;
        };
        matches.push(TokenMatch {
            start: found.start() - 1,
            end: found.end(),
            kind: TokenKind::Hashtag,
        });
    }
    matches
}

fn token_priority(kind: TokenKind) -> u8 {
    match kind {
        TokenKind::Reference => 0,
        TokenKind::Url => 1,
        TokenKind::Hashtag => 2,
    }
}

fn push_text(
    nodes: &mut Vec<InlineNode>,
    text: &str,
    range: Range<usize>,
    base: usize,
    styles: &[InlineStyle],
) {
    if range.is_empty() {
        return;
    }
    nodes.push(InlineNode::Text {
        text: text[range.clone()].to_string(),
        source: SourceRange::from_usize(base + range.start, base + range.end),
        styles: styles.to_vec(),
    });
}

fn split_text_line_breaks(nodes: &mut Vec<InlineNode>) {
    let original = std::mem::take(nodes);
    for node in original {
        let InlineNode::Text {
            text,
            source,
            styles,
        } = node
        else {
            nodes.push(node);
            continue;
        };
        let mut cursor = 0usize;
        for (index, _) in text.match_indices('\n') {
            if index > cursor {
                nodes.push(InlineNode::Text {
                    text: text[cursor..index].to_string(),
                    source: SourceRange::from_usize(
                        source.start as usize + cursor,
                        source.start as usize + index,
                    ),
                    styles: styles.clone(),
                });
            }
            nodes.push(InlineNode::HardBreak {
                source: SourceRange::from_usize(
                    source.start as usize + index,
                    source.start as usize + index + 1,
                ),
            });
            cursor = index + 1;
        }
        if cursor < text.len() {
            nodes.push(InlineNode::Text {
                text: text[cursor..].to_string(),
                source: SourceRange::from_usize(
                    source.start as usize + cursor,
                    source.end as usize,
                ),
                styles,
            });
        }
    }
}

fn reference_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(
            r"(?i)(?:nostr:)?(?:npub|nprofile|note|nevent|naddr)1[023456789acdefghjklmnpqrstuvwxyz]+",
        )
        .expect("reference regex is valid")
    })
}

fn url_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"https?://[^\s<>]+").expect("url regex is valid"))
}

fn hashtag_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(r"(?u)(?:^|[\s\(\[\{])#(?P<tag>[\p{L}\p{N}_]+)").expect("hashtag regex is valid")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const NPUB: &str = "npub14f8usejl26twx0dhuxjh9cas7keav9vr0v8nvtwtrjqx3vycc76qqh9nsy";

    #[test]
    fn mixed_plain_content_preserves_source_and_reference_semantics() {
        let source = format!("hello nostr:{NPUB}, see #Nostr and https://example.com.");
        let document = parse_content(&source, ContentSyntax::PlainText);
        assert_eq!(document.blocks.len(), 1);
        assert_eq!(document.references().len(), 1);
        let reference = document.references()[0];
        assert_eq!(
            &source[reference.source.start as usize..reference.source.end as usize],
            reference.original
        );
        assert!(matches!(reference.target, ReferenceTarget::Profile { .. }));
        assert!(document.blocks[0].inlines.iter().any(|node| matches!(
            node,
            InlineNode::Hashtag { hashtag, .. } if hashtag == "nostr"
        )));
        assert!(document.blocks[0].inlines.iter().any(|node| matches!(
            node,
            InlineNode::Link { destination, .. } if destination == "https://example.com"
        )));
    }

    #[test]
    fn standalone_reference_is_distinct_from_inline_placement() {
        let document = parse_content(NPUB, ContentSyntax::PlainText);
        assert_eq!(
            document.references()[0].placement,
            ReferencePlacement::Standalone
        );

        let inline = parse_content(&format!("hello {NPUB}"), ContentSyntax::PlainText);
        assert_eq!(inline.references()[0].placement, ReferencePlacement::Inline);
    }

    #[test]
    fn duplicate_occurrences_share_target_key_but_not_occurrence_id() {
        let document = parse_content(&format!("{NPUB} and {NPUB}"), ContentSyntax::PlainText);
        let references = document.references();
        assert_eq!(references.len(), 2);
        assert_ne!(references[0].id, references[1].id);
        assert_eq!(references[0].target.key(), references[1].target.key());
    }

    #[test]
    fn malformed_reference_remains_visible_and_reports_diagnostic() {
        let source = "hello nostr:npub1notvalid";
        let document = parse_content(source, ContentSyntax::PlainText);
        assert!(matches!(
            document.diagnostics.as_slice(),
            [ContentDiagnostic::MalformedReference { .. }]
        ));
        let visible: String = document.blocks[0]
            .inlines
            .iter()
            .filter_map(|node| match node {
                InlineNode::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(visible, source);
    }

    #[test]
    fn markdown_keeps_structure_internal_and_finds_inline_reference() {
        let source = format!("# Heading\n\nHello **{NPUB}**.\n\n- first\n- second");
        let document = parse_content(&source, ContentSyntax::Markdown);
        assert!(matches!(
            document.blocks[0].kind,
            BlockKind::Heading { level: 1 }
        ));
        assert_eq!(document.references().len(), 1);
        assert!(document
            .blocks
            .iter()
            .any(|block| matches!(block.kind, BlockKind::ListItem { .. })));
        let reference_styles = document
            .blocks
            .iter()
            .flat_map(|block| block.inlines.iter())
            .find_map(|node| match node {
                InlineNode::Reference { styles, .. } => Some(styles),
                _ => None,
            })
            .expect("reference node");
        assert!(reference_styles.contains(&InlineStyle::Strong));
    }

    #[test]
    fn parser_truncation_is_utf8_safe_and_explicit() {
        let source = "é".repeat(MAX_CONTENT_BYTES);
        let document = parse_content(&source, ContentSyntax::PlainText);
        assert!(matches!(
            document.diagnostics.first(),
            Some(ContentDiagnostic::InputTruncated { .. })
        ));
    }

    proptest! {
        #[test]
        fn arbitrary_unicode_never_panics_and_all_ranges_stay_on_utf8_boundaries(
            chars in proptest::collection::vec(any::<char>(), 0..512)
        ) {
            let source: String = chars.into_iter().collect();
            for syntax in [ContentSyntax::PlainText, ContentSyntax::Markdown] {
                let document = parse_content(&source, syntax);
                for block in &document.blocks {
                    prop_assert!(range_is_valid(block.source, &source));
                    for node in &block.inlines {
                        prop_assert!(range_is_valid(node.source(), &source));
                    }
                }
                for diagnostic in &document.diagnostics {
                    if let ContentDiagnostic::MalformedReference { source: range, .. } = diagnostic {
                        prop_assert!(range_is_valid(*range, &source));
                    }
                }
            }
        }
    }

    fn range_is_valid(range: SourceRange, source: &str) -> bool {
        let start = range.start as usize;
        let end = range.end as usize;
        start <= end
            && end <= source.len()
            && source.is_char_boundary(start)
            && source.is_char_boundary(end)
    }
}
