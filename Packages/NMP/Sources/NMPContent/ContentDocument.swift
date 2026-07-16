import Foundation
import NMPFFI

/// The authored syntax the caller or owning protocol module selected.
/// NMP never guesses an arbitrary event kind's syntax.
public enum NostrContentSyntax: Sendable, Hashable {
    case plainText
    case markdown
}

/// A half-open UTF-8 byte range into the exact original content.
public struct NostrContentSourceRange: Sendable, Hashable {
    public let start: UInt32
    public let end: UInt32

    public init(start: UInt32, end: UInt32) {
        self.start = start
        self.end = end
    }
}

/// Semantic context authored around a run of inline content. These cases are
/// document facts, not separate SwiftUI components an app is expected to use.
public enum NostrContentBlockContext: Sendable, Hashable {
    case paragraph
    case heading(level: UInt8)
    case quote(depth: UInt8)
    case listItem(ordered: Bool, ordinal: UInt64?, depth: UInt8)
    case code(language: String?)
    case thematicBreak
}

public enum NostrContentInlineStyle: Sendable, Hashable {
    case emphasis
    case strong
    case strikethrough
    case code
}

public enum NostrReferencePlacement: Sendable, Hashable {
    case inline
    case standalone
}

/// A normalized public Nostr target. Optional relay/author/kind values remain
/// acquisition hints where NIP-19 defines them as hints.
public enum NostrReferenceTarget: Sendable, Hashable {
    case profile(pubkey: String, relayHints: [String] = [])
    case event(
        id: String,
        authorHint: String? = nil,
        kindHint: UInt16? = nil,
        relayHints: [String] = []
    )
    case address(kind: UInt16, author: String, identifier: String, relayHints: [String] = [])

    /// Stable semantic identity. Hints deliberately do not change identity.
    public var key: String {
        switch self {
        case .profile(let pubkey, _):
            return "profile:\(pubkey)"
        case .event(let id, _, _, _):
            return "event:\(id)"
        case .address(let kind, let author, let identifier, _):
            return "address:\(kind):\(author):\(identifier)"
        }
    }
}

/// One authored occurrence. Several occurrences may point at one target while
/// retaining independent source identity for selection, highlighting, and UI.
public struct NostrReferenceOccurrence: Sendable, Hashable, Identifiable {
    public let id: UInt64
    public let original: String
    public let target: NostrReferenceTarget
    public let source: NostrContentSourceRange
    public let placement: NostrReferencePlacement

    public init(
        id: UInt64,
        original: String,
        target: NostrReferenceTarget,
        source: NostrContentSourceRange,
        placement: NostrReferencePlacement
    ) {
        self.id = id
        self.original = original
        self.target = target
        self.source = source
        self.placement = placement
    }
}

/// One semantic inline run. Rendering remains native and app-selectable.
public enum NostrContentInline: Sendable, Hashable {
    case text(text: String, source: NostrContentSourceRange, styles: [NostrContentInlineStyle])
    case reference(occurrence: NostrReferenceOccurrence, styles: [NostrContentInlineStyle])
    case hashtag(
        hashtag: String,
        original: String,
        source: NostrContentSourceRange,
        styles: [NostrContentInlineStyle]
    )
    case link(
        destination: String,
        label: String,
        source: NostrContentSourceRange,
        styles: [NostrContentInlineStyle]
    )
    case softBreak(source: NostrContentSourceRange)
    case hardBreak(source: NostrContentSourceRange)

    public var source: NostrContentSourceRange {
        switch self {
        case .text(_, let source, _),
             .hashtag(_, _, let source, _),
             .link(_, _, let source, _),
             .softBreak(let source),
             .hardBreak(let source):
            return source
        case .reference(let occurrence, _):
            return occurrence.source
        }
    }
}

public struct NostrContentBlock: Sendable, Hashable, Identifiable {
    public let id: UInt64
    public let context: NostrContentBlockContext
    public let source: NostrContentSourceRange
    public let inlines: [NostrContentInline]

    public init(
        id: UInt64,
        context: NostrContentBlockContext,
        source: NostrContentSourceRange,
        inlines: [NostrContentInline]
    ) {
        self.id = id
        self.context = context
        self.source = source
        self.inlines = inlines
    }
}

public enum NostrContentDiagnostic: Sendable, Hashable {
    case inputTruncated(originalBytes: UInt64, parsedBytes: UInt64)
    case malformedReference(original: String, source: NostrContentSourceRange)
}

/// Pure, immutable output of the shared Rust parser. It has no query, view,
/// renderer, navigation, or media policy hidden inside it.
public struct NostrContentDocument: Sendable, Hashable {
    public let syntax: NostrContentSyntax
    public let blocks: [NostrContentBlock]
    public let diagnostics: [NostrContentDiagnostic]

    public init(
        syntax: NostrContentSyntax,
        blocks: [NostrContentBlock],
        diagnostics: [NostrContentDiagnostic] = []
    ) {
        self.syntax = syntax
        self.blocks = blocks
        self.diagnostics = diagnostics
    }

    public var references: [NostrReferenceOccurrence] {
        blocks.flatMap(\.inlines).compactMap { inline in
            guard case .reference(let occurrence, _) = inline else { return nil }
            return occurrence
        }
    }
}

/// Parse without opening a query or performing I/O.
public func parseNostrContent(
    _ content: String,
    syntax: NostrContentSyntax = .plainText
) -> NostrContentDocument {
    NostrContentDocument(parseNostrContent(content: content, syntax: syntax.ffiValue))
}

extension NostrContentSyntax {
    fileprivate var ffiValue: FfiContentSyntax {
        switch self {
        case .plainText: return .plainText
        case .markdown: return .markdown
        }
    }

    fileprivate init(_ ffi: FfiContentSyntax) {
        switch ffi {
        case .plainText: self = .plainText
        case .markdown: self = .markdown
        }
    }
}

extension NostrContentSourceRange {
    fileprivate init(_ ffi: FfiSourceRange) {
        self.init(start: ffi.start, end: ffi.end)
    }
}

extension NostrContentBlockContext {
    fileprivate init(_ ffi: FfiBlockKind) {
        switch ffi {
        case .paragraph: self = .paragraph
        case .heading(let level): self = .heading(level: level)
        case .quote(let depth): self = .quote(depth: depth)
        case .listItem(let ordered, let ordinal, let depth):
            self = .listItem(ordered: ordered, ordinal: ordinal, depth: depth)
        case .code(let language): self = .code(language: language)
        case .thematicBreak: self = .thematicBreak
        }
    }
}

extension NostrContentInlineStyle {
    fileprivate init(_ ffi: FfiInlineStyle) {
        switch ffi {
        case .emphasis: self = .emphasis
        case .strong: self = .strong
        case .strikethrough: self = .strikethrough
        case .code: self = .code
        }
    }
}

extension NostrReferencePlacement {
    fileprivate init(_ ffi: FfiReferencePlacement) {
        switch ffi {
        case .inline: self = .inline
        case .standalone: self = .standalone
        }
    }
}

extension NostrReferenceTarget {
    init(_ ffi: FfiReferenceTarget) {
        switch ffi {
        case .profile(let pubkey, let relayHints):
            self = .profile(pubkey: pubkey, relayHints: relayHints)
        case .event(let id, let authorHint, let kindHint, let relayHints):
            self = .event(
                id: id,
                authorHint: authorHint,
                kindHint: kindHint,
                relayHints: relayHints
            )
        case .address(let kind, let author, let identifier, let relayHints):
            self = .address(
                kind: kind,
                author: author,
                identifier: identifier,
                relayHints: relayHints
            )
        }
    }

}

extension NostrReferenceOccurrence {
    fileprivate init(_ ffi: FfiReferenceOccurrence) {
        self.init(
            id: ffi.id,
            original: ffi.original,
            target: NostrReferenceTarget(ffi.target),
            source: NostrContentSourceRange(ffi.source),
            placement: NostrReferencePlacement(ffi.placement)
        )
    }
}

extension NostrContentInline {
    fileprivate init(_ ffi: FfiInlineNode) {
        switch ffi {
        case .text(let text, let source, let styles):
            self = .text(
                text: text,
                source: NostrContentSourceRange(source),
                styles: styles.map(NostrContentInlineStyle.init)
            )
        case .reference(let occurrence, let styles):
            self = .reference(
                occurrence: NostrReferenceOccurrence(occurrence),
                styles: styles.map(NostrContentInlineStyle.init)
            )
        case .hashtag(let hashtag, let original, let source, let styles):
            self = .hashtag(
                hashtag: hashtag,
                original: original,
                source: NostrContentSourceRange(source),
                styles: styles.map(NostrContentInlineStyle.init)
            )
        case .link(let destination, let label, let source, let styles):
            self = .link(
                destination: destination,
                label: label,
                source: NostrContentSourceRange(source),
                styles: styles.map(NostrContentInlineStyle.init)
            )
        case .softBreak(let source):
            self = .softBreak(source: NostrContentSourceRange(source))
        case .hardBreak(let source):
            self = .hardBreak(source: NostrContentSourceRange(source))
        }
    }
}

extension NostrContentDiagnostic {
    fileprivate init(_ ffi: FfiContentDiagnostic) {
        switch ffi {
        case .inputTruncated(let originalBytes, let parsedBytes):
            self = .inputTruncated(originalBytes: originalBytes, parsedBytes: parsedBytes)
        case .malformedReference(let original, let source):
            self = .malformedReference(
                original: original,
                source: NostrContentSourceRange(source)
            )
        }
    }
}

extension NostrContentDocument {
    fileprivate init(_ ffi: FfiContentDocument) {
        self.init(
            syntax: NostrContentSyntax(ffi.syntax),
            blocks: ffi.blocks.map { block in
                NostrContentBlock(
                    id: block.id,
                    context: NostrContentBlockContext(block.kind),
                    source: NostrContentSourceRange(block.source),
                    inlines: block.inlines.map(NostrContentInline.init)
                )
            },
            diagnostics: ffi.diagnostics.map(NostrContentDiagnostic.init)
        )
    }
}
