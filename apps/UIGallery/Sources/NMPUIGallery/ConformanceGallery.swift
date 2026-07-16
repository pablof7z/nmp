import NMP
import NMPContent
import NMPUI
import SwiftUI

struct ConformanceGallery: View {
    let model: GalleryModel
    @State private var reducedMotionReaction = false

    var body: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 28) {
                GalleryIntro(
                    eyebrow: "REPEATABLE CONFORMANCE",
                    title: "Every awkward state stays useful.",
                    description: "These deterministic component policies and native environment probes keep fallback, accessibility, and rendering behavior inspectable."
                )

                GallerySection("Reference component policies", note: "The same parsed target can remain literal, render without an engine, or stop at an immutable cycle boundary. None requires a document session or shared coordinator.") {
                    VStack(alignment: .leading, spacing: 12) {
                        policyRow("Literal · zero fetch") {
                            NostrContent(
                                document: model.profileDocument,
                                observationFactory: model.observationFactory,
                                renderers: .literalReferences,
                                maximumBlocks: 1,
                                maximumLinesPerBlock: 1
                            )
                        }
                        policyRow("No factory") {
                            NostrContent(
                                document: model.profileDocument,
                                maximumBlocks: 1,
                                maximumLinesPerBlock: 1
                            )
                        }
                        policyRow("Cycle boundary") {
                            NostrContent(
                                document: model.noteDocument,
                                observationFactory: model.observationFactory,
                                context: cycleContext,
                                purpose: .preview,
                                maximumBlocks: 1,
                                maximumLinesPerBlock: 1
                            )
                        }
                    }
                }
                .accessibilityIdentifier("gallery.states.reference-policies")

                GallerySection("Unknown kind", note: "A deterministic app loader supplies kind:55555 to the same actual-kind table. With no exact registration, generic event chrome remains readable.") {
                    NostrContent(
                        document: model.noteDocument,
                        purpose: .embedded,
                        renderers: unknownKindRenderers
                    )
                }
                .accessibilityIdentifier("gallery.states.unknown-kind")

                GallerySection("Accessibility Dynamic Type", note: "The real user-card composition is forced to Accessibility 3. Text grows; controls and identity do not overlap or clip.") {
                    NMPUserCard(
                        pubkey: Self.placeholderPubkey,
                        profile: Self.longProfile,
                        variant: .landscape
                    )
                    .dynamicTypeSize(.accessibility3)
                }
                .accessibilityIdentifier("gallery.states.dynamic-type")

                GallerySection("Follow action states", note: "The production controlled primitive renders the closed NMP state vocabulary. These fixtures make every state inspectable; live user cards use the connected NMPFollowing resource.") {
                    VStack(alignment: .leading, spacing: 14) {
                        HStack(spacing: 10) {
                            NMPFollowButtonBody(
                                snapshot: followSnapshot(.notFollowing, .ready),
                                variant: .compact,
                                action: {}
                            )
                            NMPFollowButtonBody(
                                snapshot: followSnapshot(.following, .ready),
                                variant: .compact,
                                action: {}
                            )
                            NMPFollowButtonBody(
                                snapshot: followSnapshot(.notFollowing, .ready),
                                isActing: true,
                                variant: .icon,
                                action: {}
                            )
                        }
                        NMPFollowButtonBody(
                            snapshot: followSnapshot(.notFollowing, .ready),
                            variant: .prominent,
                            action: {}
                        )
                        NMPFollowButtonBody(
                            snapshot: followSnapshot(.notFollowing, .ready),
                            offersAnotherAttempt: true,
                            variant: .prominent,
                            action: {}
                        )
                        HStack(spacing: 10) {
                            NMPFollowButtonBody(
                                snapshot: followSnapshot(.unknown, .acquiring),
                                action: {}
                            )
                            NMPFollowButtonBody(
                                snapshot: followSnapshot(.following, .cachedOnly),
                                action: {}
                            )
                            NMPFollowButtonBody(
                                snapshot: followSnapshot(.notFollowing, .noContactList),
                                action: {}
                            )
                            NMPFollowButtonBody(
                                snapshot: followSnapshot(.unknown, .signedOut),
                                variant: .icon,
                                action: {}
                            )
                        }
                    }
                }
                .accessibilityIdentifier("gallery.states.follow")

                GallerySection("Right-to-left flow", note: "Native layout direction reverses identity, text, and reaction ordering without a parallel renderer.") {
                    HStack(spacing: 12) {
                        NMPProfileMention(
                            pubkey: Self.placeholderPubkey,
                            profile: Self.longProfile,
                            variant: .pill
                        )
                        NMPReactionButton(
                            isReacted: true,
                            count: 12,
                            variant: .minimal,
                            action: {}
                        )
                    }
                    .environment(\.layoutDirection, .rightToLeft)
                }
                .accessibilityIdentifier("gallery.states.rtl")

                GallerySection("Reduced motion", note: "This button uses the production component with Reduce Motion forced on; state changes without the spring pulse.") {
                    NMPReactionButton(
                        isReacted: reducedMotionReaction,
                        count: reducedMotionReaction ? 43 : 42,
                        variant: .heart,
                        motion: .reduced
                    ) {
                        reducedMotionReaction.toggle()
                    }
                }
                .accessibilityIdentifier("gallery.states.reduced-motion")

                GallerySection("Dark appearance", note: "Semantic theme tokens and disabled remote-image policy preserve contrast and hierarchy.") {
                    NMPUserCard(
                        pubkey: Self.placeholderPubkey,
                        profile: Self.longProfile,
                        variant: .featured
                    )
                    .padding(12)
                    .background(Color.black, in: RoundedRectangle(cornerRadius: 22))
                    .environment(\.colorScheme, .dark)
                }
                .accessibilityIdentifier("gallery.states.dark")

                GallerySection("Avatar group primitive", note: "A shared leaf handles overlap and honest overflow instead of each reaction or social card rebuilding it.") {
                    NMPAvatarGroup(
                        people: Self.avatarPeople,
                        maximumVisible: 4,
                        size: 36
                    )
                }
                .accessibilityIdentifier("gallery.states.avatar-group")

                GallerySection("Long Markdown", note: "Headings, inline native mentions, lists, quotes, code, and long wrapping exercise the pure document walk at production text sizes.") {
                    NostrContent(
                        document: model.longContentDocument,
                        observationFactory: model.observationFactory,
                        purpose: .detail,
                        renderers: .standard
                    )
                }
                .accessibilityIdentifier("gallery.states.long-content")
            }
            .padding(20)
        }
        .navigationTitle("States")
        .navigationBarTitleDisplayMode(.inline)
    }

    private var cycleContext: NostrContentRenderContext {
        let key = model.noteDocument.references.first?.target.key ?? "missing"
        return NostrContentRenderContext(
            ancestorTargetKeys: [key],
            depth: 1,
            maximumDepth: 3
        )
    }

    private var unknownKindRenderers: NostrContentRenderers {
        NostrContentRenderers.standard.eventLoader { input in
            if let context = input.context.descending(into: input.occurrence.target.key) {
                NMPResolvedEventDispatcher(
                    reference: input,
                    event: Self.unknownEvent,
                    context: context
                )
            } else {
                Text("recursive embed stopped")
            }
        }
    }

    private func policyRow<Content: View>(
        _ label: String,
        @ViewBuilder content: () -> Content
    ) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 12) {
            Text(label)
                .font(.caption.weight(.semibold))
                .frame(width: 124, alignment: .leading)
            content()
        }
    }

    private func followSnapshot(
        _ relationship: NMPFollowRelationship,
        _ availability: NMPFollowAvailability
    ) -> NMPFollowingSnapshot {
        NMPFollowingSnapshot(
            activePubkey: availability == .signedOut ? nil : Self.placeholderPubkey,
            target: Self.placeholderPubkey,
            relationship: relationship,
            availability: availability,
            baseEventID: relationship == .unknown || availability == .noContactList
                ? nil
                : String(repeating: "01", count: 32)
        )
    }

    private static let placeholderPubkey = String(repeating: "7a", count: 32)

    private static let longProfile = NMPProfilePresentation(
        name: "aurora",
        displayName: "Aurora Over the Aegean",
        about: "A deliberately long biography that exercises wrapping, large accessibility text, follow controls, and deterministic avatar fallbacks without relying on the network.",
        nip05: "aurora@example.com"
    )

    private static let avatarPeople = [
        NMPAvatarItem(pubkey: placeholderPubkey, profile: longProfile),
        NMPAvatarItem(pubkey: String(repeating: "2b", count: 32)),
        NMPAvatarItem(pubkey: String(repeating: "3c", count: 32)),
        NMPAvatarItem(pubkey: String(repeating: "4d", count: 32)),
        NMPAvatarItem(pubkey: String(repeating: "5e", count: 32)),
        NMPAvatarItem(pubkey: String(repeating: "6f", count: 32))
    ]

    private static let unknownEvent = Row(
        id: String(repeating: "55", count: 32),
        pubkey: placeholderPubkey,
        createdAt: 1_700_000_000,
        kind: 55_555,
        tags: [],
        content: "Unknown kinds retain generic event chrome and readable content.",
        sig: String(repeating: "aa", count: 64),
        sources: []
    )
}
