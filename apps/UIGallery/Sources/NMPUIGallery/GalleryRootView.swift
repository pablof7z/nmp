import NMP
import NMPContent
import NMPUI
import SwiftUI

struct GalleryRootView: View {
    let model: GalleryModel

    var body: some View {
        TabView {
            NavigationStack { ComponentsGallery(model: model) }
                .tabItem { Label("Components", systemImage: "square.grid.2x2") }
            NavigationStack { ContentGallery(model: model) }
                .tabItem { Label("Content", systemImage: "text.quote") }
            NavigationStack { LiveProofGallery(model: model) }
                .tabItem { Label("Live proof", systemImage: "dot.radiowaves.left.and.right") }
            NavigationStack { ConformanceGallery(model: model) }
                .tabItem { Label("States", systemImage: "checklist") }
            NavigationStack { StressGallery(model: model) }
                .tabItem { Label("Stress", systemImage: "gauge.with.dots.needle.67percent") }
        }
        .tint(Color(red: 0.45, green: 0.22, blue: 0.93))
        .nmpImageLoader(.system)
    }
}

private struct ComponentsGallery: View {
    let model: GalleryModel
    @State private var liked = false
    @State private var sparks = false
    @State private var selectedEmoji: Set<String> = ["🔥"]

    var body: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 28) {
                GalleryIntro(
                    eyebrow: "NMP UI · SWIFTUI",
                    title: "Useful before customization.",
                    description: "Display leaves accept app/protocol-owner values. Reference components independently choose whether to observe."
                )

                GallerySection("Identity primitives", note: "App-supplied presentation fields keep the leaf catalog inspectable without putting a kind:0 codec in NMPContent or NMPUI.") {
                    VStack(alignment: .leading, spacing: 16) {
                        HStack(spacing: 14) {
                            ForEach([24.0, 36.0, 52.0, 72.0], id: \.self) { size in
                                NMPAvatar(
                                    pubkey: GalleryModel.profilePubkey,
                                    profile: GalleryFixtures.profile,
                                    size: size
                                )
                            }
                        }
                        NMPName(
                            pubkey: GalleryModel.profilePubkey,
                            profile: GalleryFixtures.profile
                        )
                        .font(.title2.weight(.bold))
                        NMPProfileIdentity(
                            pubkey: GalleryModel.profilePubkey,
                            profile: GalleryFixtures.profile,
                            avatarSize: 44
                        )
                    }
                }

                GallerySection("Channel preview", note: "This ordinary content component owns a visibility-scoped profile observation; the parser and parent document own none.") {
                    HStack(spacing: 12) {
                        Circle()
                            .fill(Color(red: 0.80, green: 0.82, blue: 0.25))
                            .frame(width: 48, height: 48)
                            .overlay(Text("2").font(.headline).foregroundStyle(.white))
                        VStack(alignment: .leading, spacing: 3) {
                            Text("29er-next").font(.headline)
                            NostrContent(
                                document: model.previewDocument,
                                observationFactory: model.observationFactory,
                                purpose: .preview,
                                maximumBlocks: 1,
                                maximumLinesPerBlock: 1
                            )
                            .font(.subheadline)
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                        }
                        Spacer()
                        Text("4h").font(.caption).foregroundStyle(.tertiary)
                        Image(systemName: "chevron.right").foregroundStyle(.tertiary)
                    }
                    .padding(14)
                    .background(Color.primary.opacity(0.045), in: RoundedRectangle(cornerRadius: 16))
                }

                GallerySection("Featured users", note: "Three layouts, one Avatar/Name fallback contract, and one live NMP-owned NIP-02 relationship.") {
                    VStack(spacing: 14) {
                        NMPUserCard(
                            pubkey: GalleryModel.profilePubkey,
                            profile: GalleryFixtures.profile,
                            variant: .featured,
                            following: model.following
                        )
                        NMPUserCard(
                            pubkey: GalleryModel.profilePubkey,
                            profile: GalleryFixtures.profile,
                            variant: .landscape,
                            following: model.following
                        )
                        NMPUserCard(
                            pubkey: GalleryModel.profilePubkey,
                            profile: GalleryFixtures.profile,
                            variant: .compact,
                            following: model.following
                        )
                    }
                }

                GallerySection("Reaction controls", note: "Different interaction models retain controlled app-owned write state.") {
                    VStack(alignment: .leading, spacing: 18) {
                        HStack(spacing: 18) {
                            NMPReactionButton(
                                isReacted: liked,
                                count: liked ? 128 : 127,
                                variant: .heart
                            ) { liked.toggle() }
                            NMPReactionButton(
                                isReacted: sparks,
                                count: sparks ? 9 : 8,
                                variant: .spark
                            ) { sparks.toggle() }
                            NMPReactionButton(
                                isReacted: liked,
                                count: 24,
                                variant: .minimal
                            ) { liked.toggle() }
                        }
                        NMPAvatarReactionButton(
                            people: [
                                NMPReactionPerson(
                                    pubkey: GalleryModel.profilePubkey,
                                    profile: GalleryFixtures.profile
                                )
                            ],
                            totalCount: liked ? 2 : 1,
                            isReacted: liked
                        ) { liked.toggle() }
                        NMPEmojiReactionBar(
                            reactions: ["🔥", "🤙", "😂", "💜"].map {
                                NMPEmojiReaction(
                                    emoji: $0,
                                    count: baseCount($0) + (selectedEmoji.contains($0) ? 1 : 0),
                                    isSelected: selectedEmoji.contains($0)
                                )
                            },
                            select: { emoji in
                                if selectedEmoji.contains(emoji) {
                                    selectedEmoji.remove(emoji)
                                } else {
                                    selectedEmoji.insert(emoji)
                                }
                            },
                            add: {}
                        )
                    }
                }

                GallerySection("State lab · missing metadata", note: "Deterministic non-network states stay separate from the live reference example.") {
                    VStack(alignment: .leading, spacing: 16) {
                        NMPProfileIdentity(
                            pubkey: String(repeating: "7a", count: 32),
                            profile: nil,
                            avatarSize: 48
                        )
                        NMPArticleMediumCard(article: GalleryFixtures.article)
                    }
                }
            }
            .padding(20)
        }
        .navigationTitle("Components")
        .navigationBarTitleDisplayMode(.inline)
    }

    private func baseCount(_ emoji: String) -> Int {
        ["🔥": 18, "🤙": 6, "😂": 11, "💜": 4][emoji] ?? 0
    }
}

private struct ContentGallery: View {
    let model: GalleryModel

    var body: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 28) {
                GalleryIntro(
                    eyebrow: "REFERENCE POLICY",
                    title: "Detection is not acquisition.",
                    description: "The same parsed documents use literal, standard, or custom loader components without changing NMPContent."
                )
                GallerySection("Mention variants", note: "These app-selected components render the same detected profile in three visual styles and intentionally open no query.") {
                    VStack(alignment: .leading, spacing: 14) {
                        NostrContent(
                            document: model.profileDocument,
                            renderers: mentionRenderers(.text, preview: false)
                        )
                        NostrContent(
                            document: model.profileDocument,
                            renderers: mentionRenderers(.avatar, preview: true)
                        )
                        NostrContent(
                            document: model.profileDocument,
                            renderers: mentionRenderers(.pill, preview: true)
                        )
                    }
                }
                GallerySection("Article primitives", note: "Image, title, summary, byline, and reading time consume an explicit presentation value supplied by an app or exact protocol owner.") {
                    VStack(alignment: .leading, spacing: 10) {
                        NMPArticleImage(article: GalleryFixtures.article)
                            .frame(height: 128)
                            .clipShape(RoundedRectangle(cornerRadius: 14))
                        NMPArticleTitle(article: GalleryFixtures.article)
                            .font(.headline)
                        NMPArticleSummary(article: GalleryFixtures.article)
                            .font(.subheadline)
                        NMPArticleByline(
                            article: GalleryFixtures.article,
                            authorProfile: GalleryFixtures.profile
                        )
                        NMPArticleReadingTime(article: GalleryFixtures.article)
                            .font(.caption)
                    }
                }
                GallerySection("Article · portrait", note: "The production composition remains available without making NMPUI a NIP-23 schema owner.") {
                    NMPArticlePortraitCard(
                        article: GalleryFixtures.article,
                        authorProfile: GalleryFixtures.profile
                    )
                }
                GallerySection("Article · Medium-like", note: "A separate horizontal composition for lists and inline article mentions.") {
                    NMPArticleMediumCard(
                        article: GalleryFixtures.article,
                        authorProfile: GalleryFixtures.profile
                    )
                }
                GallerySection("Literal profile · zero fetch", note: "Even with a live factory available, this component ignores it.") {
                    NostrContent(
                        document: model.profileDocument,
                        observationFactory: model.observationFactory,
                        renderers: .literalReferences
                    )
                }
                GallerySection("Literal event · zero fetch", note: "The nevent remains authored text and creates no event query.") {
                    NostrContent(
                        document: model.noteDocument,
                        observationFactory: model.observationFactory,
                        renderers: .literalReferences
                    )
                }
                GallerySection("Default event loader", note: "The outer component observes; the inner table dispatches the acquired row's actual kind.") {
                    NostrContent(
                        document: model.noteDocument,
                        observationFactory: model.observationFactory,
                        purpose: .embedded,
                        renderers: editorialNoteRenderers
                    )
                }
                GallerySection("Raw kind:30023 fallback", note: "No Swift NIP-23 schema decoder is recreated while the Rust owner is missing.") {
                    NostrContent(
                        document: model.articleDocument,
                        observationFactory: model.observationFactory,
                        purpose: .card
                    )
                }
                GallerySection("Replaceable outer loader", note: "This loader chooses literal rendering while preserving the same actual-kind renderer table.") {
                    NostrContent(
                        document: model.noteDocument,
                        observationFactory: model.observationFactory,
                        renderers: NostrContentRenderers.standard.eventLoader { input in
                            NMPReferenceLiteral(original: "Fetch disabled: \(input.occurrence.original)")
                        }
                    )
                }
                GallerySection("Mixed document", note: "Each selected component owns an independent handle; equal core demands still coalesce.") {
                    NostrContent(
                        document: model.mixedDocument,
                        observationFactory: model.observationFactory
                    )
                }
                GallerySection("Immutable recursion context", note: "Long Markdown renders without a session-shaped coordinator.") {
                    NostrContent(
                        document: model.longContentDocument,
                        observationFactory: model.observationFactory
                    )
                }
            }
            .padding(20)
        }
        .navigationTitle("Content")
        .navigationBarTitleDisplayMode(.inline)
    }

    private func mentionRenderers(
        _ variant: NMPMentionVariant,
        preview: Bool
    ) -> NostrContentRenderers {
        NostrContentRenderers.standard.profileReference { input in
            NMPProfileMention(
                pubkey: input.pubkey,
                profile: GalleryFixtures.profile,
                variant: variant,
                showsLongPressPreview: preview
            )
        }
    }

    private var editorialNoteRenderers: NostrContentRenderers {
        NostrContentRenderers.standard.event(kind: 1, layout: .block) { input in
            NMPEventChrome(
                pubkey: input.event.pubkey,
                createdAt: input.event.createdAt,
                variant: .editorial
            ) {
                Text(input.event.content)
            }
        }
    }
}

private enum GalleryFixtures {
    static let profile = NMPProfilePresentation(
        name: "alice",
        displayName: "Alice",
        about: "Presentation fields are supplied by an app or exact protocol owner.",
        nip05: "alice@example.com"
    )

    static let article = NMPArticlePresentation(
        author: GalleryModel.profilePubkey,
        createdAt: UInt64(Date().timeIntervalSince1970),
        title: "Article cards consume presentation values, not a Swift NIP-23 codec",
        summary: "The standard event loader stays on raw rows until the exact Rust owner exists.",
        content: Array(repeating: "word", count: 360).joined(separator: " ")
    )
}

private struct LiveProofGallery: View {
    let model: GalleryModel
    @State private var diagnostics = DiagnosticsSnapshot()

    var body: some View {
        List {
            Section("Visible component-owned observations") {
                NostrContent(
                    document: model.previewDocument,
                    observationFactory: model.observationFactory,
                    purpose: .preview,
                    maximumBlocks: 1,
                    maximumLinesPerBlock: 1
                )
                NostrContent(
                    document: model.noteDocument,
                    observationFactory: model.observationFactory,
                    purpose: .embedded
                )
            }
            Section("Engine relay diagnostics") {
                if diagnostics.relays.isEmpty {
                    HStack { ProgressView(); Text("Waiting for visible demand…") }
                }
                ForEach(diagnostics.relays) { relay in
                    VStack(alignment: .leading, spacing: 4) {
                        Text(relay.relay).font(.caption.monospaced()).lineLimit(1)
                        Text("\(relay.wireSubCount) wire subs")
                            .font(.caption2)
                            .foregroundStyle(.secondary)
                    }
                }
            }
        }
        .navigationTitle("Live proof")
        .task { await observeDiagnostics() }
    }

    private func observeDiagnostics() async {
        guard let stream = try? model.engine.observeDiagnostics() else { return }
        for await snapshot in stream {
            diagnostics = snapshot
        }
    }
}

struct GalleryIntro: View {
    let eyebrow: String
    let title: String
    let description: String

    var body: some View {
        VStack(alignment: .leading, spacing: 9) {
            Text(eyebrow)
                .font(.caption2.weight(.bold))
                .tracking(1.2)
                .foregroundStyle(Color(red: 0.36, green: 0.08, blue: 0.68))
            Text(title)
                .font(.largeTitle.weight(.bold))
            Text(description)
                .font(.subheadline)
                .foregroundStyle(Color.primary.opacity(0.64))
        }
    }
}

struct GallerySection<Content: View>: View {
    let title: String
    let note: String
    let content: Content

    init(_ title: String, note: String, @ViewBuilder content: () -> Content) {
        self.title = title
        self.note = note
        self.content = content()
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 13) {
            VStack(alignment: .leading, spacing: 4) {
                Text(title).font(.title3.weight(.bold))
                Text(note).font(.caption).foregroundStyle(Color.primary.opacity(0.64))
            }
            content
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}
