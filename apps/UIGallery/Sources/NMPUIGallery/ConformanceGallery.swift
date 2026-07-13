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
                    description: "These are deterministic, network-free sessions and native environment probes. They make failure, accessibility, and fallback behavior inspectable instead of implied."
                )

                GallerySection("Scripted reference states", note: "No engine exists behind these sessions. Loading, shortfall, and cycle states remain visible and open no query.") {
                    VStack(alignment: .leading, spacing: 12) {
                        stateRow("Loading", session: model.loadingSession)
                        stateRow("No planned source", session: model.shortfallSession)
                        stateRow("Cycle boundary", session: model.cycleSession)
                    }
                }
                .accessibilityIdentifier("gallery.states.scripted")

                GallerySection("Unknown kind", note: "A made-up kind:55555 receives generic event chrome and readable nested content until this app installs an exact renderer.") {
                    NostrContent(
                        session: model.unknownKindSession,
                        purpose: .embedded,
                        renderers: .standard
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

                GallerySection("Follow action states", note: "The production controlled primitive renders the closed NMP state vocabulary. These fixtures make every state inspectable; the live user cards use the connected NMPFollowing resource.") {
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

                GallerySection("Reduced motion", note: "This button uses the same production component with Reduce Motion forced on; state changes without the spring pulse.") {
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

                GallerySection("Long Markdown", note: "Headings, inline native mentions, lists, quotes, code, and long wrapping exercise the full content walk at production text sizes.") {
                    NostrContent(
                        session: model.longContentSession,
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

    private func stateRow(_ label: String, session: NostrContentSession) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 12) {
            Text(label)
                .font(.caption.weight(.semibold))
                .frame(width: 116, alignment: .leading)
            NostrContent(
                session: session,
                purpose: .preview,
                renderers: .standard,
                maximumBlocks: 1,
                maximumLinesPerBlock: 1
            )
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

    private static let longProfile = NostrProfileMetadata(
        pubkey: placeholderPubkey,
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
}
