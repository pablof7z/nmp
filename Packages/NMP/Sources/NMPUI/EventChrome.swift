import NMP
import NMPContent
import SwiftUI

public enum NMPEventChromeVariant: Sendable, Hashable {
    case compact
    case standard
    case editorial
}

/// Reusable event shell. It deliberately accepts arbitrary content and footer
/// views, so the same chrome can host NostrContent, an app-only kind, media, or
/// an entirely custom interaction surface.
public struct NMPEventChrome<Content: View, Footer: View>: View {
    @Environment(\.nmpUITheme) private var theme

    public let pubkey: String
    public let profile: NostrProfileMetadata?
    public let createdAt: UInt64
    public let variant: NMPEventChromeVariant
    private let content: Content
    private let footer: Footer

    public init(
        pubkey: String,
        profile: NostrProfileMetadata? = nil,
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
        .overlay(RoundedRectangle(cornerRadius: theme.cornerRadius).strokeBorder(theme.border, lineWidth: 0.5))
    }

    private var editorial: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(alignment: .center, spacing: 12) {
                NMPAvatar(pubkey: pubkey, profile: profile, size: 52)
                header(font: .title3)
            }
            content
                .font(.system(.body, design: .serif))
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
        profile: NostrProfileMetadata? = nil,
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

/// Connected convenience that claims kind:0 through an existing content
/// session. It never opens an independent query or owns a cache.
public struct NMPResolvedProfile<Content: View>: View {
    @ObservedObject private var session: NostrContentSession
    @State private var claim: NostrContentClaim?

    public let pubkey: String
    private let content: (NostrProfileMetadata?) -> Content

    public init(
        session: NostrContentSession,
        pubkey: String,
        @ViewBuilder content: @escaping (NostrProfileMetadata?) -> Content
    ) {
        self.session = session
        self.pubkey = pubkey
        self.content = content
    }

    public var body: some View {
        content(profile)
            .onAppear {
                if claim == nil { claim = session.claimProfile(pubkey: pubkey) }
            }
            .onDisappear {
                claim?.cancel()
                claim = nil
            }
    }

    private var profile: NostrProfileMetadata? {
        session.snapshot.state(for: .profile(pubkey: pubkey)).resource?.profile
    }
}

struct NMPResolvedEventCard: View {
    let input: NMPEventRenderInput
    let variant: NMPEventChromeVariant

    var body: some View {
        NMPResolvedProfile(session: input.session, pubkey: input.event.pubkey) { profile in
            NMPEventChrome(
                pubkey: input.event.pubkey,
                profile: profile,
                createdAt: input.event.createdAt,
                variant: variant
            ) {
                NMPNestedEventContent(input: input)
            } footer: {
                HStack(spacing: 18) {
                    Label("Reply", systemImage: "bubble.left")
                    Label("React", systemImage: "heart")
                    Spacer()
                    Text("kind \(input.event.kind)")
                }
                .font(.caption)
                .foregroundStyle(.secondary)
            }
            .contentShape(Rectangle())
            .onTapGesture { input.actions.openEvent(input.event) }
        }
    }
}

struct NMPNestedEventContent: View {
    @StateObject private var nestedSession: NostrContentSession
    let input: NMPEventRenderInput

    init(input: NMPEventRenderInput) {
        self.input = input
        _nestedSession = StateObject(
            wrappedValue: input.session.client.session(
                content: input.event.content,
                syntax: input.event.kind == 30_023 ? .markdown : .plainText,
                policy: input.session.policy,
                context: input.context
            )
        )
    }

    var body: some View {
        NostrContent(
            session: nestedSession,
            purpose: .embedded,
            renderers: input.renderers,
            actions: input.actions
        )
        .onDisappear { nestedSession.stop() }
    }
}

enum NMPResolvedArticleVariant {
    case portrait
    case medium
}

struct NMPResolvedArticleCard: View {
    let input: NMPEventRenderInput
    let variant: NMPResolvedArticleVariant

    var body: some View {
        if let article = decodeNIP23Article(from: input.event) {
            NMPResolvedProfile(session: input.session, pubkey: article.author) { profile in
                switch variant {
                case .portrait:
                    NMPArticlePortraitCard(
                        article: article,
                        authorProfile: profile,
                        action: { input.actions.openEvent(input.event) }
                    )
                case .medium:
                    NMPArticleMediumCard(
                        article: article,
                        authorProfile: profile,
                        action: { input.actions.openEvent(input.event) }
                    )
                }
            }
        } else {
            NMPResolvedEventCard(input: input, variant: .standard)
        }
    }
}

struct NMPReferencePlaceholder: View {
    @Environment(\.nmpUITheme) private var theme
    let input: NMPReferenceFallbackInput

    var body: some View {
        HStack(spacing: 6) {
            if isLoading { ProgressView().controlSize(.mini) }
            Image(systemName: icon)
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

    private var isLoading: Bool {
        switch input.state {
        case .idle, .loading, .refreshing: return true
        case .resolved, .withdrawn, .shortfall, .stopped, .collapsed: return false
        }
    }

    private var icon: String {
        switch input.state {
        case .shortfall, .stopped, .withdrawn: return "exclamationmark.circle"
        case .collapsed: return "arrow.triangle.branch"
        case .idle, .loading, .refreshing, .resolved: return "link"
        }
    }

    private var compactOriginal: String {
        let original = input.occurrence.original
        guard original.count > 28 else { return original }
        return "\(original.prefix(18))…\(original.suffix(7))"
    }
}
