import NMPContent
import SwiftUI

/// Pure document walk plus app-selected components. Reference detection does
/// not attach a modifier, open an observation, or imply network work.
public struct NostrContent: View {
    public let document: NostrContentDocument
    /// Capability offered to app-selected reference components. Supplying it
    /// does not cause `NostrContent` or the document walk to observe anything.
    public let observationFactory: NMPReferenceObservationFactory?
    /// Immutable cycle/depth ancestry for nested event components.
    public let context: NostrContentRenderContext
    public let purpose: NostrContentPurpose
    public let renderers: NostrContentRenderers
    public let actions: NostrContentActions
    public let maximumBlocks: Int?
    public let maximumLinesPerBlock: Int?

    public init(
        document: NostrContentDocument,
        observationFactory: NMPReferenceObservationFactory? = nil,
        context: NostrContentRenderContext = .root,
        purpose: NostrContentPurpose = .body,
        renderers: NostrContentRenderers = .standard,
        actions: NostrContentActions = NostrContentActions(),
        maximumBlocks: Int? = nil,
        maximumLinesPerBlock: Int? = nil
    ) {
        self.document = document
        self.observationFactory = observationFactory
        self.context = context
        self.purpose = purpose
        self.renderers = renderers
        self.actions = actions
        self.maximumBlocks = maximumBlocks.map { max(1, $0) }
        self.maximumLinesPerBlock = maximumLinesPerBlock.map { max(1, $0) }
    }

    public init(
        content: String,
        syntax: NostrContentSyntax = .plainText,
        observationFactory: NMPReferenceObservationFactory? = nil,
        context: NostrContentRenderContext = .root,
        purpose: NostrContentPurpose = .body,
        renderers: NostrContentRenderers = .standard,
        actions: NostrContentActions = NostrContentActions(),
        maximumBlocks: Int? = nil,
        maximumLinesPerBlock: Int? = nil
    ) {
        self.init(
            document: parseNostrContent(content, syntax: syntax),
            observationFactory: observationFactory,
            context: context,
            purpose: purpose,
            renderers: renderers,
            actions: actions,
            maximumBlocks: maximumBlocks,
            maximumLinesPerBlock: maximumLinesPerBlock
        )
    }

    public var body: some View {
        VStack(alignment: .leading, spacing: blockSpacing) {
            ForEach(visibleBlocks) { block in
                blockView(block)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    @ViewBuilder
    private func blockView(_ block: NostrContentBlock) -> some View {
        switch block.context {
        case .paragraph:
            flow(block)
        case .heading(let level):
            flow(block)
                .font(headingFont(level))
                .padding(.top, level <= 2 ? 7 : 3)
        case .quote:
            HStack(alignment: .top, spacing: 11) {
                Capsule().fill(Color.secondary.opacity(0.34)).frame(width: 3)
                flow(block).foregroundStyle(.secondary)
            }
        case .listItem(let ordered, let ordinal, let depth):
            HStack(alignment: .top, spacing: 8) {
                Text(ordered ? "\(ordinal ?? 1)." : "•")
                    .font(.body.weight(.semibold))
                    .frame(width: 18, alignment: .trailing)
                flow(block)
            }
            .padding(.leading, CGFloat(depth) * 14)
        case .code:
            flow(block)
                .font(.system(.callout, design: .monospaced))
                .padding(12)
                .background(Color.secondary.opacity(0.08), in: RoundedRectangle(cornerRadius: 10))
        case .thematicBreak:
            Divider().padding(.vertical, 6)
        }
    }

    private func flow(_ block: NostrContentBlock) -> some View {
        NMPFlowLayout(
            horizontalSpacing: 0,
            verticalSpacing: 5,
            maximumLines: maximumLinesPerBlock
        ) {
            ForEach(fragments(for: block)) { fragment in
                fragment.view
                    .layoutValue(key: NMPFlowRoleKey.self, value: fragment.role)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func fragments(for block: NostrContentBlock) -> [Fragment] {
        if !block.inlines.contains(where: \.requiresNativeFlow) {
            return [
                Fragment(
                    id: "\(block.id)-text-runs",
                    role: .inline,
                    view: AnyView(
                        NMPStyledTextRuns(
                            inlines: block.inlines,
                            maximumLines: maximumLinesPerBlock
                        )
                    )
                )
            ]
        }

        var result: [Fragment] = []
        for (inlineIndex, inline) in block.inlines.enumerated() {
            switch inline {
            case .text(let text, let source, let styles):
                for (pieceIndex, piece) in Self.textPieces(text).enumerated() {
                    result.append(
                        Fragment(
                            id: "\(block.id)-\(inlineIndex)-text-\(source.start)-\(pieceIndex)",
                            role: piece == "\n" ? .breakLine : .inline,
                            view: AnyView(
                                NMPStyledText(text: piece == "\n" ? "" : piece, styles: styles)
                            )
                        )
                    )
                }
            case .reference(let occurrence, _):
                let node = renderedReference(occurrence)
                result.append(
                    Fragment(
                        id: Self.referenceFragmentID(blockID: block.id, occurrence: occurrence),
                        node: node
                    )
                )
            case .hashtag(let hashtag, let original, _, _):
                let node = renderers.renderHashtag(
                    NMPHashtagRenderInput(
                        hashtag: hashtag,
                        original: original,
                        purpose: purpose,
                        actions: actions
                    )
                )
                result.append(Fragment(id: "\(block.id)-hashtag-\(inlineIndex)", node: node))
            case .link(let destination, let label, _, _):
                let node = renderers.renderLink(
                    NMPLinkRenderInput(
                        destination: destination,
                        label: label,
                        purpose: purpose,
                        actions: actions
                    )
                )
                result.append(Fragment(id: "\(block.id)-link-\(inlineIndex)", node: node))
            case .softBreak:
                result.append(
                    Fragment(
                        id: "\(block.id)-soft-break-\(inlineIndex)",
                        role: .inline,
                        view: AnyView(Text(" ").fixedSize())
                    )
                )
            case .hardBreak:
                result.append(
                    Fragment(
                        id: "\(block.id)-hard-break-\(inlineIndex)",
                        role: .breakLine,
                        view: AnyView(Color.clear.frame(width: 0, height: 0))
                    )
                )
            }
        }
        return result
    }

    private func renderedReference(_ occurrence: NostrReferenceOccurrence) -> NMPRenderedNode {
        switch occurrence.target {
        case .profile(let pubkey, _):
            return renderers.renderProfileReference(
                NMPProfileReferenceInput(
                    occurrence: occurrence,
                    pubkey: pubkey,
                    purpose: purpose,
                    observationFactory: observationFactory,
                    actions: actions
                )
            )
        case .event, .address:
            return renderers.renderEventReference(
                NMPEventReferenceInput(
                    occurrence: occurrence,
                    purpose: purpose,
                    context: context,
                    observationFactory: observationFactory,
                    renderers: renderers,
                    actions: actions
                )
            )
        }
    }

    private func headingFont(_ level: UInt8) -> Font {
        switch level {
        case 1: return .largeTitle.bold()
        case 2: return .title.bold()
        case 3: return .title2.bold()
        default: return .headline.bold()
        }
    }

    private var blockSpacing: CGFloat {
        switch purpose {
        case .preview: return 5
        case .body, .embedded, .card, .detail: return 10
        }
    }

    private var visibleBlocks: [NostrContentBlock] {
        guard let maximumBlocks else { return document.blocks }
        return Array(document.blocks.prefix(maximumBlocks))
    }

    private static func textPieces(_ text: String) -> [String] {
        guard !text.isEmpty else { return [] }
        var result: [String] = []
        var current = ""
        var currentIsWhitespace: Bool?

        func flush() {
            guard !current.isEmpty else { return }
            result.append(current)
            current = ""
        }

        for character in text {
            if character == "\n" {
                flush()
                result.append("\n")
                currentIsWhitespace = nil
                continue
            }
            let whitespace = character.isWhitespace
            if let currentIsWhitespace, currentIsWhitespace != whitespace {
                flush()
            }
            currentIsWhitespace = whitespace
            current.append(character)
        }
        flush()
        return result
    }

    static func referenceFragmentID(
        blockID: UInt64,
        occurrence: NostrReferenceOccurrence
    ) -> String {
        "\(blockID)-reference-\(occurrence.id)-\(occurrence.target.key)"
    }

    private struct Fragment: Identifiable {
        let id: String
        let role: NMPFlowRole
        let view: AnyView

        init(id: String, role: NMPFlowRole, view: AnyView) {
            self.id = id
            self.role = role
            self.view = view
        }

        init(id: String, node: NMPRenderedNode) {
            self.init(
                id: id,
                role: node.layout == .block ? .block : .inline,
                view: node.view
            )
        }
    }
}

private struct NMPStyledText: View {
    let text: String
    let styles: [NostrContentInlineStyle]

    var body: some View {
        styledText
            .padding(.horizontal, styles.contains(.code) ? 3 : 0)
            .background(
                styles.contains(.code) ? Color.secondary.opacity(0.10) : .clear,
                in: RoundedRectangle(cornerRadius: 3)
            )
    }

    private var styledText: Text {
        var value = Text(text)
        if styles.contains(.code) {
            value = value.font(.system(.body, design: .monospaced))
        }
        if styles.contains(.strong) { value = value.bold() }
        if styles.contains(.emphasis) { value = value.italic() }
        if styles.contains(.strikethrough) { value = value.strikethrough() }
        return value
    }
}

private struct NMPStyledTextRuns: View {
    let inlines: [NostrContentInline]
    let maximumLines: Int?

    var body: some View {
        composed.lineLimit(maximumLines)
    }

    private var composed: Text {
        inlines.reduce(Text("")) { partial, inline in
            switch inline {
            case .text(let text, _, let styles):
                return partial + styled(text, styles: styles)
            case .softBreak:
                return partial + Text(" ")
            case .hardBreak:
                return partial + Text("\n")
            case .reference, .hashtag, .link:
                return partial
            }
        }
    }

    private func styled(_ value: String, styles: [NostrContentInlineStyle]) -> Text {
        var text = Text(value)
        if styles.contains(.code) {
            text = text.font(.system(.body, design: .monospaced))
        }
        if styles.contains(.strong) { text = text.bold() }
        if styles.contains(.emphasis) { text = text.italic() }
        if styles.contains(.strikethrough) { text = text.strikethrough() }
        return text
    }
}

private extension NostrContentInline {
    var requiresNativeFlow: Bool {
        switch self {
        case .reference, .hashtag, .link: return true
        case .text, .softBreak, .hardBreak: return false
        }
    }
}
