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
                    description: "Real public components, backed by one live NMP engine. Each section is the same API an app imports."
                )

                GallerySection("Identity primitives", note: "Kind:0 resolves live; every size has a deterministic fallback before it arrives.") {
                    NMPResolvedProfile(session: model.profileSession, pubkey: GalleryModel.profilePubkey) { profile in
                        VStack(alignment: .leading, spacing: 16) {
                            HStack(spacing: 14) {
                                ForEach([24.0, 36.0, 52.0, 72.0], id: \.self) { size in
                                    NMPAvatar(
                                        pubkey: GalleryModel.profilePubkey,
                                        profile: profile,
                                        size: size
                                    )
                                }
                            }
                            NMPName(pubkey: GalleryModel.profilePubkey, profile: profile)
                                .font(.title2.weight(.bold))
                            NMPProfileIdentity(
                                pubkey: GalleryModel.profilePubkey,
                                profile: profile,
                                avatarSize: 44
                            )
                        }
                    }
                }

                GallerySection("Channel preview", note: "The preview receives ordinary Nostr content; the bech32 is replaced in place as profile metadata arrives.") {
                    HStack(spacing: 12) {
                        Circle()
                            .fill(Color(red: 0.80, green: 0.82, blue: 0.25))
                            .frame(width: 48, height: 48)
                            .overlay(Text("2").font(.headline).foregroundStyle(.white))
                        VStack(alignment: .leading, spacing: 3) {
                            Text("29er-next").font(.headline)
                            NostrContent(
                                session: model.previewSession,
                                purpose: .preview,
                                renderers: nameOnlyRenderers
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

                GallerySection("Featured users", note: "Three layouts, one Avatar/Name fallback contract, app-controlled follow state.") {
                    NMPResolvedProfile(session: model.profileSession, pubkey: GalleryModel.profilePubkey) { profile in
                        VStack(spacing: 14) {
                            NMPUserCard(
                                pubkey: GalleryModel.profilePubkey,
                                profile: profile,
                                variant: .featured,
                                followAction: {}
                            )
                            NMPUserCard(
                                pubkey: GalleryModel.profilePubkey,
                                profile: profile,
                                variant: .landscape,
                                isFollowing: true,
                                followAction: {}
                            )
                            NMPUserCard(
                                pubkey: GalleryModel.profilePubkey,
                                profile: profile,
                                variant: .compact,
                                followAction: {}
                            )
                        }
                    }
                }

                GallerySection("Reaction controls", note: "Different interaction models—not one button with three padding values. State and writes remain controlled by the host app.") {
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

                        NMPResolvedProfile(session: model.profileSession, pubkey: GalleryModel.profilePubkey) { profile in
                            NMPAvatarReactionButton(
                                people: [
                                    NMPReactionPerson(
                                        pubkey: GalleryModel.profilePubkey,
                                        profile: profile
                                    )
                                ],
                                totalCount: liked ? 2 : 1,
                                isReacted: liked
                            ) { liked.toggle() }
                        }

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

                GallerySection("State lab · missing metadata", note: "Deterministic non-network states are kept separate from the live examples above.") {
                    VStack(alignment: .leading, spacing: 16) {
                        NMPProfileIdentity(
                            pubkey: String(repeating: "7a", count: 32),
                            profile: nil,
                            avatarSize: 48
                        )
                        NMPArticleMediumCard(article: missingImageArticle)
                    }
                }
            }
            .padding(20)
        }
        .navigationTitle("Components")
        .navigationBarTitleDisplayMode(.inline)
    }

    private var nameOnlyRenderers: NostrContentRenderers {
        NostrContentRenderers.standard.profileMention { input in
            NMPProfileMention(
                pubkey: input.pubkey,
                profile: input.profile,
                variant: .text
            )
        }
    }

    private func baseCount(_ emoji: String) -> Int {
        ["🔥": 18, "🤙": 6, "😂": 11, "💜": 4][emoji] ?? 0
    }

    private var missingImageArticle: NostrArticle {
        NostrArticle(
            eventID: String(repeating: "01", count: 32),
            author: String(repeating: "7a", count: 32),
            createdAt: UInt64(Date().timeIntervalSince1970),
            identifier: "state-lab-missing-image",
            title: "An article still has a useful shape before its image arrives",
            summary: "The same real component preserves hierarchy and interaction continuity.",
            content: Array(repeating: "word", count: 360).joined(separator: " ")
        )
    }
}

private struct ContentGallery: View {
    let model: GalleryModel

    var body: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 28) {
                GalleryIntro(
                    eyebrow: "LIVE CONTENT",
                    title: "One document. Native views inside the words.",
                    description: "NMP parses and resolves. This renderer set decides how each occurrence looks—locally, immutably, and without leaf-owned queries."
                )

                GallerySection("Mention variants", note: "All three resolve the same real profile. Long-press the avatar or pill variants.") {
                    VStack(alignment: .leading, spacing: 14) {
                        NostrContent(
                            session: model.profileSession,
                            renderers: mentionRenderers(.text, preview: false)
                        )
                        NostrContent(
                            session: model.profileSession,
                            renderers: mentionRenderers(.avatar, preview: true)
                        )
                        NostrContent(
                            session: model.profileSession,
                            renderers: mentionRenderers(.pill, preview: true)
                        )
                    }
                }

                GallerySection("Article · portrait", note: "Featured editorial layout with lead image, title, summary, reading time, and live author identity.") {
                    NostrContent(
                        session: model.articleSession,
                        purpose: .card,
                        renderers: .standard
                    )
                }

                GallerySection("Article · Medium-like", note: "A separate horizontal composition intended for lists and inline article mentions.") {
                    NostrContent(
                        session: model.articleSession,
                        purpose: .embedded,
                        renderers: .standard
                    )
                }

                GallerySection("Generic event chrome", note: "Author profile and nested event content resolve through the parent content session.") {
                    NostrContent(
                        session: model.noteSession,
                        purpose: .embedded,
                        renderers: .standard
                    )
                }

                GallerySection("Local renderer override", note: "This call site replaces kind:1 only. No global registration and no Rust/FFI enum change.") {
                    NostrContent(
                        session: model.noteSession,
                        purpose: .embedded,
                        renderers: editorialNoteRenderers
                    )
                }

                GallerySection("Mixed document", note: "Inline mention, standalone article, and quoted event share one parsed document and deduplicated live demands.") {
                    NostrContent(
                        session: model.mixedSession,
                        purpose: .body,
                        renderers: .standard
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
        NostrContentRenderers.standard.profileMention { input in
            NMPProfileMention(
                pubkey: input.pubkey,
                profile: input.profile,
                variant: variant,
                showsLongPressPreview: preview
            )
        }
    }

    private var editorialNoteRenderers: NostrContentRenderers {
        NostrContentRenderers.standard.event(kind: 1, layout: .block) { input in
            NMPResolvedProfile(session: input.session, pubkey: input.event.pubkey) { profile in
                NMPEventChrome(
                    pubkey: input.event.pubkey,
                    profile: profile,
                    createdAt: input.event.createdAt,
                    variant: .editorial
                ) {
                    Text(input.event.content)
                }
            }
        }
    }
}

private struct LiveProofGallery: View {
    let model: GalleryModel
    @State private var diagnostics = DiagnosticsSnapshot()
    @State private var proofClaims: [NostrContentClaim] = []

    var body: some View {
        List {
            Section {
                VStack(alignment: .leading, spacing: 8) {
                    Text("Only two relay facts enter the app")
                        .font(.headline)
                    Text("Everything else below—including author outboxes for the hint-free article and note—is NMP’s ordinary routing work.")
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                }
                .padding(.vertical, 4)
                ForEach(GalleryModel.indexers, id: \.self) { relay in
                    Label(relay, systemImage: "server.rack")
                        .font(.callout.monospaced())
                }
            } header: {
                Text("Configured indexers")
            }

            Section("Live content state") {
                stateRow("Profile", state: model.profileSession.snapshot.resources.values.first)
                stateRow("No-hint article", state: model.articleSession.snapshot.resources.values.first)
                stateRow("No-hint note", state: model.noteSession.snapshot.resources.values.first)
            }

            Section("Engine relay diagnostics") {
                if diagnostics.relays.isEmpty {
                    HStack { ProgressView(); Text("Waiting for live relay plans…") }
                }
                ForEach(diagnostics.relays) { relay in
                    VStack(alignment: .leading, spacing: 5) {
                        Text(relay.relay).font(.caption.monospaced()).lineLimit(1)
                        HStack {
                            Text("\(relay.wireSubCount) wire subs")
                            Text("·")
                            Text("\(relay.eventsByKind.reduce(0) { $0 + $1.count }) events")
                        }
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        if !GalleryModel.indexers.contains(relay.relay) {
                            Text("discovered by NMP")
                                .font(.caption2.weight(.semibold))
                                .foregroundStyle(.purple)
                        }
                    }
                }
            }

            Section("Hardcoded seed entities") {
                seed("profile · relay hint", GalleryModel.profile)
                seed("article · no relay hint", GalleryModel.article)
                seed("note · author hint only", GalleryModel.note)
            }
        }
        .navigationTitle("Live proof")
        .task { await observeDiagnostics() }
        .onAppear { startProofClaims() }
        .onDisappear {
            proofClaims.forEach { $0.cancel() }
            proofClaims.removeAll()
        }
    }

    private func stateRow(_ label: String, state: NostrReferenceState?) -> some View {
        HStack {
            Text(label)
            Spacer()
            Text(stateLabel(state))
                .font(.caption.weight(.semibold))
                .foregroundStyle(state?.resource == nil ? Color.secondary : Color.green)
        }
    }

    private func stateLabel(_ state: NostrReferenceState?) -> String {
        guard let state else { return "idle" }
        switch state {
        case .idle: return "idle"
        case .loading: return "loading"
        case .refreshing: return "refreshing cache"
        case .resolved: return "resolved"
        case .withdrawn: return "withdrawn"
        case .shortfall: return "shortfall"
        case .stopped: return "stopped"
        case .collapsed: return "bounded"
        }
    }

    private func seed(_ label: String, _ value: String) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(label).font(.caption.weight(.semibold))
            Text(value).font(.caption2.monospaced()).lineLimit(2)
        }
    }

    private func observeDiagnostics() async {
        guard let stream = try? model.engine.observeDiagnostics() else { return }
        for await snapshot in stream {
            diagnostics = snapshot
        }
    }

    private func startProofClaims() {
        guard proofClaims.isEmpty else { return }
        proofClaims = [model.profileSession, model.articleSession, model.noteSession].compactMap { session in
            guard let id = session.snapshot.document.references.first?.id else { return nil }
            return session.claim(referenceID: id)
        }
    }
}

private struct GalleryIntro: View {
    let eyebrow: String
    let title: String
    let description: String

    var body: some View {
        VStack(alignment: .leading, spacing: 9) {
            Text(eyebrow)
                .font(.caption2.weight(.bold))
                .tracking(1.2)
                .foregroundStyle(.purple)
            Text(title)
                .font(.largeTitle.weight(.bold))
            Text(description)
                .font(.subheadline)
                .foregroundStyle(.secondary)
        }
    }
}

private struct GallerySection<Content: View>: View {
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
                Text(note).font(.caption).foregroundStyle(.secondary)
            }
            content
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}
