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

/// A component returns an arbitrary SwiftUI view plus its document-flow role.
public struct NMPRenderedNode {
    let view: AnyView
    let layout: NMPContentNodeLayout

    public init<Content: View>(
        _ layout: NMPContentNodeLayout = .inline,
        @ViewBuilder content: () -> Content
    ) {
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

/// Input to the app-selected profile-reference component. Receiving a factory
/// does not open anything; only the selected component can call it.
public struct NMPProfileReferenceInput {
    public let occurrence: NostrReferenceOccurrence
    public let pubkey: String
    public let purpose: NostrContentPurpose
    public let observationFactory: NMPReferenceObservationFactory?
    public let actions: NostrContentActions
}

/// Input to the app-selected outer event-reference loader. The loader may
/// observe, render literally, ask for consent, or apply another app policy.
public struct NMPEventReferenceInput {
    public let occurrence: NostrReferenceOccurrence
    public let purpose: NostrContentPurpose
    public let context: NostrContentRenderContext
    public let observationFactory: NMPReferenceObservationFactory?
    public let renderers: NostrContentRenderers
    public let actions: NostrContentActions
}

/// Input delivered only after an outer loader has acquired a row. Renderer
/// selection uses `event.kind`, never the occurrence's optional kind hint.
public struct NMPEventRenderInput {
    public let occurrence: NostrReferenceOccurrence
    public let event: Row
    public let purpose: NostrContentPurpose
    public let context: NostrContentRenderContext
    public let observationFactory: NMPReferenceObservationFactory?
    public let renderers: NostrContentRenderers
    public let actions: NostrContentActions
}

public struct NMPReferenceFallbackInput {
    public let occurrence: NostrReferenceOccurrence
    public let purpose: NostrContentPurpose
    public let failure: String?
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

/// Immutable, locally scoped component table. The outer event loader and the
/// actual-kind renderer table are independent extension points.
public struct NostrContentRenderers {
    private struct EventKey: Hashable {
        let kind: UInt16
        let purpose: NostrContentPurpose?
    }

    private var profileReferenceComponent: (NMPProfileReferenceInput) -> NMPRenderedNode
    private var eventReferenceLoader: (NMPEventReferenceInput) -> NMPRenderedNode
    private var eventRenderers: [EventKey: (NMPEventRenderInput) -> NMPRenderedNode]
    private var eventFallback: (NMPEventRenderInput) -> NMPRenderedNode
    private var referenceFallback: (NMPReferenceFallbackInput) -> NMPRenderedNode
    private var hashtagRenderer: (NMPHashtagRenderInput) -> NMPRenderedNode
    private var linkRenderer: (NMPLinkRenderInput) -> NMPRenderedNode

    public static var standard: NostrContentRenderers {
        NostrContentRenderers()
            .event(kind: 1, layout: .block) { input in
                NMPGenericEventCard(input: input, label: "Note")
            }
            .event(kind: 30_023, layout: .block) { input in
                // NIP-23 schema projection intentionally waits for its exact
                // Rust owner. The standard component renders the validated
                // raw row honestly in the meantime.
                NMPGenericEventCard(input: input, label: "Long-form event")
            }
    }

    /// A complete no-observation component choice useful for literal/link UI.
    public static var literalReferences: NostrContentRenderers {
        NostrContentRenderers()
            .profileReferenceNode { input in
                NMPRenderedNode(
                    input.occurrence.placement == .standalone ? .block : .inline
                ) {
                    NMPReferenceLiteral(original: input.occurrence.original)
                }
            }
            .eventLoaderNode { input in
                NMPRenderedNode(
                    input.occurrence.placement == .standalone ? .block : .inline
                ) {
                    NMPReferenceLiteral(original: input.occurrence.original)
                }
            }
    }

    public init() {
        profileReferenceComponent = { input in
            NMPRenderedNode(.inline) {
                NMPStandardProfileMention(input: input)
            }
        }
        eventReferenceLoader = { input in
            NMPRenderedNode(input.occurrence.placement == .standalone ? .block : .inline) {
                NMPDefaultEventLoader(input: input)
            }
        }
        eventRenderers = [:]
        eventFallback = { input in
            NMPRenderedNode(.block) {
                NMPGenericEventCard(input: input, label: "Event kind \(input.event.kind)")
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

    /// Replace the profile-reference component, including its decision to
    /// observe or render only the authored occurrence.
    public func profileReference<Content: View>(
        layout: NMPContentNodeLayout = .inline,
        @ViewBuilder _ component: @escaping (NMPProfileReferenceInput) -> Content
    ) -> NostrContentRenderers {
        var copy = self
        copy.profileReferenceComponent = { input in
            NMPRenderedNode(layout) { component(input) }
        }
        return copy
    }

    /// Node-level form for components whose flow role depends on occurrence
    /// placement rather than one fixed registration-time layout.
    public func profileReferenceNode(
        _ component: @escaping (NMPProfileReferenceInput) -> NMPRenderedNode
    ) -> NostrContentRenderers {
        var copy = self
        copy.profileReferenceComponent = component
        return copy
    }

    /// Replace the outer event loader without changing any actual-kind
    /// renderer. The loader owns whether and how acquisition occurs.
    public func eventLoader<Content: View>(
        layout: NMPContentNodeLayout = .block,
        @ViewBuilder _ component: @escaping (NMPEventReferenceInput) -> Content
    ) -> NostrContentRenderers {
        var copy = self
        copy.eventReferenceLoader = { input in
            NMPRenderedNode(layout) { component(input) }
        }
        return copy
    }

    /// Node-level form for outer loaders whose flow role depends on the
    /// authored occurrence. It changes no actual-kind renderer entry.
    public func eventLoaderNode(
        _ component: @escaping (NMPEventReferenceInput) -> NMPRenderedNode
    ) -> NostrContentRenderers {
        var copy = self
        copy.eventReferenceLoader = component
        return copy
    }

    /// Register presentation for a validated acquired row's actual kind and
    /// optional purpose. This renderer never decides acquisition.
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

    func renderProfileReference(_ input: NMPProfileReferenceInput) -> NMPRenderedNode {
        profileReferenceComponent(input)
    }

    func renderEventReference(_ input: NMPEventReferenceInput) -> NMPRenderedNode {
        eventReferenceLoader(input)
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
