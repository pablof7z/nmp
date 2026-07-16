import NMP
import NMPContent
import SwiftUI

public enum NMPEventChromeVariant: Sendable, Hashable {
    case compact
    case standard
    case editorial
}

/// Reusable event shell over app/protocol-owner supplied identity display data.
public struct NMPEventChrome<Content: View, Footer: View>: View {
    @Environment(\.nmpUITheme) private var theme

    public let pubkey: String
    public let profile: NMPProfilePresentation?
    public let createdAt: UInt64
    public let variant: NMPEventChromeVariant
    private let content: Content
    private let footer: Footer

    public init(
        pubkey: String,
        profile: NMPProfilePresentation? = nil,
        createdAt: UInt64,
        variant: NMPEventChromeVariant = .standard,
        @ViewBuilder content: () -> Content,
        @ViewBuilder footer: () -> Footer
    ) {
        self.pubkey = pubkey
        self.profile = profile
        self.createdAt = createdAt
        self.variant = variant
        self.content = content()
        self.footer = footer()
    }

    public var body: some View {
        Group {
            switch variant {
            case .compact: compact
            case .standard: standard
            case .editorial: editorial
            }
        }
    }

    private var compact: some View {
        HStack(alignment: .top, spacing: 10) {
            NMPAvatar(pubkey: pubkey, profile: profile, size: 34)
            VStack(alignment: .leading, spacing: 5) {
                header(font: .subheadline)
                content
                footer
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding(.vertical, 8)
    }

    private var standard: some View {
        VStack(alignment: .leading, spacing: 11) {
            HStack(spacing: 10) {
                NMPAvatar(pubkey: pubkey, profile: profile, size: 42)
                header(font: .headline)
                Spacer(minLength: 8)
                Image(systemName: "ellipsis")
                    .foregroundStyle(theme.secondary)
                    .accessibilityHidden(true)
            }
            content
            footer
        }
        .padding(15)
        .background(theme.surface, in: RoundedRectangle(cornerRadius: theme.cornerRadius))
        .overlay(
            RoundedRectangle(cornerRadius: theme.cornerRadius)
                .strokeBorder(theme.border, lineWidth: 0.5)
        )
    }

    private var editorial: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(alignment: .center, spacing: 12) {
                NMPAvatar(pubkey: pubkey, profile: profile, size: 52)
                header(font: .title3)
            }
            content.font(.system(.body, design: .serif))
            Divider().overlay(theme.border)
            footer
        }
        .padding(20)
        .background(theme.surface, in: RoundedRectangle(cornerRadius: 22))
        .overlay(RoundedRectangle(cornerRadius: 22).strokeBorder(theme.border, lineWidth: 0.5))
    }

    private func header(font: Font) -> some View {
        VStack(alignment: .leading, spacing: 1) {
            NMPName(pubkey: pubkey, profile: profile)
                .font(font.weight(.semibold))
                .foregroundStyle(theme.foreground)
            Text(Date(timeIntervalSince1970: TimeInterval(createdAt)), style: .relative)
                .font(.caption)
                .foregroundStyle(theme.secondary)
        }
    }
}

public extension NMPEventChrome where Footer == EmptyView {
    init(
        pubkey: String,
        profile: NMPProfilePresentation? = nil,
        createdAt: UInt64,
        variant: NMPEventChromeVariant = .standard,
        @ViewBuilder content: () -> Content
    ) {
        self.init(
            pubkey: pubkey,
            profile: profile,
            createdAt: createdAt,
            variant: variant,
            content: content,
            footer: { EmptyView() }
        )
    }
}

/// Standard profile-reference component. It owns its own visibility-scoped
/// kind:0 observation when a factory is supplied. Until #208 supplies the
/// exact profile codec, the visual remains the pubkey fallback and the raw
/// row is retained only as live replacement evidence.
public struct NMPStandardProfileMention: View {
    let input: NMPProfileReferenceInput

    public init(input: NMPProfileReferenceInput) {
        self.input = input
    }

    @ViewBuilder
    public var body: some View {
        if let factory = input.observationFactory {
            NMPObservedProfileMention(input: input, factory: factory)
        } else {
            mention
        }
    }

    private var mention: some View {
        NMPProfileMention(
            pubkey: input.pubkey,
            variant: .avatar,
            showsLongPressPreview: true,
            action: { input.actions.openProfile(input.pubkey) }
        )
    }
}

private struct NMPObservedProfileMention: View {
    let input: NMPProfileReferenceInput
    @StateObject private var observation: NMPVisibleReferenceObservation

    init(input: NMPProfileReferenceInput, factory: NMPReferenceObservationFactory) {
        self.input = input
        _observation = StateObject(
            wrappedValue: NMPVisibleReferenceObservation(
                target: input.occurrence.target,
                factory: factory
            )
        )
    }

    var body: some View {
        NMPProfileMention(
            pubkey: input.pubkey,
            variant: .avatar,
            showsLongPressPreview: true,
            action: { input.actions.openProfile(input.pubkey) }
        )
        // A replacement row changes identity even while the temporary raw-row
        // fallback has no schema fields to display.
        .id(profileEvent?.id ?? input.pubkey)
        .observeWhileVisible(observation)
    }

    private var profileEvent: Row? {
        observation.canonical?.rows.first {
            $0.kind == 0 && $0.pubkey == input.pubkey
        }
    }
}

/// Standard outer event-reference component. It is replaceable independently
/// from the actual-kind renderer table.
public struct NMPDefaultEventLoader: View {
    let input: NMPEventReferenceInput

    public init(input: NMPEventReferenceInput) {
        self.input = input
    }

    @ViewBuilder
    public var body: some View {
        if let nextContext = input.context.descending(into: input.occurrence.target.key) {
            if let factory = input.observationFactory {
                NMPObservedEventLoader(
                    input: input,
                    context: nextContext,
                    factory: factory
                )
            } else {
                fallback(failure: "no observation factory")
            }
        } else {
            fallback(failure: "recursive embed stopped")
        }
    }

    private func fallback(failure: String?) -> some View {
        input.renderers.renderReferenceFallback(
            NMPReferenceFallbackInput(
                occurrence: input.occurrence,
                purpose: input.purpose,
                failure: failure
            )
        ).view
    }
}

private struct NMPObservedEventLoader: View {
    let input: NMPEventReferenceInput
    let context: NostrContentRenderContext
    @StateObject private var observation: NMPVisibleReferenceObservation

    init(
        input: NMPEventReferenceInput,
        context: NostrContentRenderContext,
        factory: NMPReferenceObservationFactory
    ) {
        self.input = input
        self.context = context
        _observation = StateObject(
            wrappedValue: NMPVisibleReferenceObservation(
                target: input.occurrence.target,
                factory: factory
            )
        )
    }

    var body: some View {
        Group {
            if let event = observation.canonical?.rows.first {
                NMPResolvedEventDispatcher(
                    reference: input,
                    event: event,
                    context: context
                )
            } else {
                input.renderers.renderReferenceFallback(
                    NMPReferenceFallbackInput(
                        occurrence: input.occurrence,
                        purpose: input.purpose,
                        failure: observation.failure
                    )
                ).view
            }
        }
        .observeWhileVisible(observation)
    }
}

/// Reusable second stage for custom outer loaders. Dispatch is always keyed by
/// the validated acquired row's actual kind and the requested purpose.
public struct NMPResolvedEventDispatcher: View {
    public let reference: NMPEventReferenceInput
    public let event: Row
    public let context: NostrContentRenderContext

    public init(
        reference: NMPEventReferenceInput,
        event: Row,
        context: NostrContentRenderContext
    ) {
        self.reference = reference
        self.event = event
        self.context = context
    }

    public var body: some View {
        reference.renderers.renderEvent(
            NMPEventRenderInput(
                occurrence: reference.occurrence,
                event: event,
                purpose: reference.purpose,
                context: context,
                observationFactory: reference.observationFactory,
                renderers: reference.renderers,
                actions: reference.actions
            )
        ).view
    }
}

struct NMPGenericEventCard: View {
    let input: NMPEventRenderInput
    let label: String

    var body: some View {
        NMPEventChrome(
            pubkey: input.event.pubkey,
            createdAt: input.event.createdAt,
            variant: .standard
        ) {
            NMPNestedEventContent(input: input)
        } footer: {
            HStack(spacing: 18) {
                Label("Reply", systemImage: "bubble.left")
                Label("React", systemImage: "heart")
                Spacer()
                Text(label)
            }
            .font(.caption)
            .foregroundStyle(.secondary)
        }
        .modifier(
            NMPCardInteraction(
                accessibilityName: "Open event",
                action: { input.actions.openEvent(input.event) }
            )
        )
    }
}

private struct NMPNestedEventContent: View {
    let input: NMPEventRenderInput

    var body: some View {
        NostrContent(
            content: input.event.content,
            // Syntax selection for kind:30023 remains presentation-only here;
            // no NIP-23 tags or schema fields are decoded in Swift.
            syntax: input.event.kind == 30_023 ? .markdown : .plainText,
            observationFactory: input.observationFactory,
            context: input.context,
            purpose: .embedded,
            renderers: input.renderers,
            actions: input.actions
        )
    }
}

public struct NMPReferenceLiteral: View {
    public let original: String

    public init(original: String) {
        self.original = original
    }

    public var body: some View {
        Text(original).font(.body.monospaced())
    }
}

struct NMPReferencePlaceholder: View {
    @Environment(\.nmpUITheme) private var theme
    let input: NMPReferenceFallbackInput

    var body: some View {
        HStack(spacing: 6) {
            if input.failure == nil { ProgressView().controlSize(.mini) }
            Image(systemName: input.failure == nil ? "link" : "exclamationmark.circle")
                .font(.caption)
            Text(compactOriginal)
                .font(.caption.monospaced())
                .lineLimit(1)
        }
        .foregroundStyle(theme.secondary)
        .padding(.horizontal, 8)
        .padding(.vertical, 5)
        .background(theme.surface, in: Capsule())
    }

    private var compactOriginal: String {
        let original = input.occurrence.original
        guard original.count > 28 else { return original }
        return "\(original.prefix(18))…\(original.suffix(7))"
    }
}
