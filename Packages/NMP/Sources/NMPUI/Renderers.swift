import NMP
import NMPContent
import SwiftUI

public enum NostrContentPurpose: Sendable, Hashable {
    case body
    case preview
    case embedded
    case card
    case detail
}

public enum NMPContentNodeLayout: Sendable, Hashable {
    case inline
    case block
}

/// A renderer returns both an arbitrary SwiftUI view and how it participates
/// in document flow. The view can be an app-only kind, not an NMP enum case.
public struct NMPRenderedNode {
    let view: AnyView
    let layout: NMPContentNodeLayout

    public init<Content: View>(_ layout: NMPContentNodeLayout = .inline, @ViewBuilder content: () -> Content) {
        self.layout = layout
        self.view = AnyView(content())
    }
}

public struct NostrContentActions {
    public var openProfile: (String) -> Void
    public var openEvent: (Row) -> Void
    public var openURL: (URL) -> Void
    public var openHashtag: (String) -> Void

    public init(
        openProfile: @escaping (String) -> Void = { _ in },
        openEvent: @escaping (Row) -> Void = { _ in },
        openURL: @escaping (URL) -> Void = { _ in },
        openHashtag: @escaping (String) -> Void = { _ in }
    ) {
        self.openProfile = openProfile
        self.openEvent = openEvent
        self.openURL = openURL
        self.openHashtag = openHashtag
    }
}

public struct NMPProfileMentionInput {
    public let occurrence: NostrReferenceOccurrence
    public let state: NostrReferenceState
    public let pubkey: String
    public let profile: NostrProfileMetadata?
    public let purpose: NostrContentPurpose
    public let actions: NostrContentActions
}

public struct NMPEventRenderInput {
    public let occurrence: NostrReferenceOccurrence
    public let state: NostrReferenceState
    public let event: Row
    public let purpose: NostrContentPurpose
    public let context: NostrContentRenderContext
    public let session: NostrContentSession
    public let renderers: NostrContentRenderers
    public let actions: NostrContentActions
}

public struct NMPReferenceFallbackInput {
    public let occurrence: NostrReferenceOccurrence
    public let state: NostrReferenceState
    public let purpose: NostrContentPurpose
}

public struct NMPHashtagRenderInput {
    public let hashtag: String
    public let original: String
    public let purpose: NostrContentPurpose
    public let actions: NostrContentActions
}

public struct NMPLinkRenderInput {
    public let destination: String
    public let label: String
    public let purpose: NostrContentPurpose
    public let actions: NostrContentActions
}

/// Immutable, locally scoped dispatch. Including/replacing returns a new value;
/// there is no global registry, component host, or callback into Rust.
public struct NostrContentRenderers {
    private struct EventKey: Hashable {
        let kind: UInt16
        let purpose: NostrContentPurpose?
    }

    private var profileRenderer: (NMPProfileMentionInput) -> NMPRenderedNode
    private var eventRenderers: [EventKey: (NMPEventRenderInput) -> NMPRenderedNode]
    private var eventFallback: (NMPEventRenderInput) -> NMPRenderedNode
    private var referenceFallback: (NMPReferenceFallbackInput) -> NMPRenderedNode
    private var hashtagRenderer: (NMPHashtagRenderInput) -> NMPRenderedNode
    private var linkRenderer: (NMPLinkRenderInput) -> NMPRenderedNode

    public static var standard: NostrContentRenderers {
        NostrContentRenderers()
            .event(kind: 30_023, purpose: .card, layout: .block) { input in
                NMPResolvedArticleCard(input: input, variant: .portrait)
            }
            .event(kind: 30_023, purpose: .detail, layout: .block) { input in
                NMPResolvedArticleCard(input: input, variant: .portrait)
            }
            .event(kind: 30_023, layout: .block) { input in
                NMPResolvedArticleCard(input: input, variant: .medium)
            }
    }

    public init() {
        profileRenderer = { input in
            NMPRenderedNode(.inline) {
                NMPProfileMention(
                    pubkey: input.pubkey,
                    profile: input.profile,
                    variant: .avatar,
                    showsLongPressPreview: true,
                    action: { input.actions.openProfile(input.pubkey) }
                )
            }
        }
        eventRenderers = [:]
        eventFallback = { input in
            NMPRenderedNode(.block) {
                NMPResolvedEventCard(input: input, variant: .standard)
            }
        }
        referenceFallback = { input in
            NMPRenderedNode(input.occurrence.placement == .standalone ? .block : .inline) {
                NMPReferencePlaceholder(input: input)
            }
        }
        hashtagRenderer = { input in
            NMPRenderedNode(.inline) {
                Button(input.original) { input.actions.openHashtag(input.hashtag) }
                    .buttonStyle(.plain)
                    .foregroundStyle(.tint)
            }
        }
        linkRenderer = { input in
            NMPRenderedNode(.inline) {
                Button(input.label) {
                    guard let url = URL(string: input.destination) else { return }
                    input.actions.openURL(url)
                }
                .buttonStyle(.plain)
                .foregroundStyle(.tint)
            }
        }
    }

    public func profileMention<Content: View>(
        layout: NMPContentNodeLayout = .inline,
        @ViewBuilder _ renderer: @escaping (NMPProfileMentionInput) -> Content
    ) -> NostrContentRenderers {
        var copy = self
        copy.profileRenderer = { input in NMPRenderedNode(layout) { renderer(input) } }
        return copy
    }

    public func event<Content: View>(
        kind: UInt16,
        purpose: NostrContentPurpose? = nil,
        layout: NMPContentNodeLayout = .block,
        @ViewBuilder _ renderer: @escaping (NMPEventRenderInput) -> Content
    ) -> NostrContentRenderers {
        var copy = self
        copy.eventRenderers[EventKey(kind: kind, purpose: purpose)] = { input in
            NMPRenderedNode(layout) { renderer(input) }
        }
        return copy
    }

    public func fallbackEvent<Content: View>(
        layout: NMPContentNodeLayout = .block,
        @ViewBuilder _ renderer: @escaping (NMPEventRenderInput) -> Content
    ) -> NostrContentRenderers {
        var copy = self
        copy.eventFallback = { input in NMPRenderedNode(layout) { renderer(input) } }
        return copy
    }

    public func unresolvedReference<Content: View>(
        layout: NMPContentNodeLayout = .inline,
        @ViewBuilder _ renderer: @escaping (NMPReferenceFallbackInput) -> Content
    ) -> NostrContentRenderers {
        var copy = self
        copy.referenceFallback = { input in NMPRenderedNode(layout) { renderer(input) } }
        return copy
    }

    public func hashtag<Content: View>(
        @ViewBuilder _ renderer: @escaping (NMPHashtagRenderInput) -> Content
    ) -> NostrContentRenderers {
        var copy = self
        copy.hashtagRenderer = { input in NMPRenderedNode(.inline) { renderer(input) } }
        return copy
    }

    public func link<Content: View>(
        @ViewBuilder _ renderer: @escaping (NMPLinkRenderInput) -> Content
    ) -> NostrContentRenderers {
        var copy = self
        copy.linkRenderer = { input in NMPRenderedNode(.inline) { renderer(input) } }
        return copy
    }

    public func hasEventRenderer(kind: UInt16, purpose: NostrContentPurpose? = nil) -> Bool {
        eventRenderers[EventKey(kind: kind, purpose: purpose)] != nil
    }

    func renderProfile(_ input: NMPProfileMentionInput) -> NMPRenderedNode {
        profileRenderer(input)
    }

    func renderEvent(_ input: NMPEventRenderInput) -> NMPRenderedNode {
        eventRenderers[EventKey(kind: input.event.kind, purpose: input.purpose)]?(input)
            ?? eventRenderers[EventKey(kind: input.event.kind, purpose: nil)]?(input)
            ?? eventFallback(input)
    }

    func renderReferenceFallback(_ input: NMPReferenceFallbackInput) -> NMPRenderedNode {
        referenceFallback(input)
    }

    func renderHashtag(_ input: NMPHashtagRenderInput) -> NMPRenderedNode {
        hashtagRenderer(input)
    }

    func renderLink(_ input: NMPLinkRenderInput) -> NMPRenderedNode {
        linkRenderer(input)
    }
}
