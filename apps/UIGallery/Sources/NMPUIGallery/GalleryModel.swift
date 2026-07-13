import Combine
import Foundation
import NMP
import NMPContent

@MainActor
final class GalleryModel: ObservableObject {
    static let indexers = ["wss://purplepag.es", "wss://relay.primal.net"]

    // Real entities. The article and note intentionally carry no relay hint;
    // their author hints let ordinary NMP outbox discovery do the routing.
    static let profile = "nprofile1qqsrhuxx8l9ex335q7he0f09aej04zpazpl0ne2cgukyawd24mayt8gpzfmhxue69uhhqatjwpkx2urpvuhx2ucl9q7qz"
    static let article = "naddr1qq3xummnw3ez6atw94shqupdv9kz6emfdaexumedwa5xjar994hx76tnv5pzpqlkerf45wg3uht9d22ym7hdj9xlnklpryzk5px0hd8nc8xu4j6aqvzqqqr4guzrk365"
    static let note = "nevent1qqsvgksrcarvjcltp7a20s3ux9d626zdj8xsdmyduqpz8dudvldl25gzyqalp33lewf5vdq847t6te0wvnags0gs0mu72kz8938tn24wlfze6fh6mfe"
    static let profilePubkey = "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d"

    let engine: NMPEngine
    let following: NMPFollowing
    let content: NMPContentClient
    let profileSession: NostrContentSession
    let articleSession: NostrContentSession
    let noteSession: NostrContentSession
    let mixedSession: NostrContentSession
    let previewSession: NostrContentSession
    let unknownKindSession: NostrContentSession
    let loadingSession: NostrContentSession
    let shortfallSession: NostrContentSession
    let cycleSession: NostrContentSession
    let longContentSession: NostrContentSession
    let stressSessions: [NostrContentSession]

    @Published private(set) var stressActiveReferences = 0
    private var cancellables: Set<AnyCancellable> = []

    init() throws {
        let caches = FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask).first
        // NMP is pre-v2 and the store deliberately makes event schemas a
        // recreate boundary. Version the disposable Gallery cache filename so
        // an installed older Gallery cannot strand this proof app at startup.
        let store = caches?.appendingPathComponent("nmp-ui-gallery-v6.redb").path
        engine = try NMPEngine(
            config: NMPConfig(storePath: store, indexerRelays: Self.indexers)
        )
        following = try NMPFollowing(engine: engine, target: Self.profilePubkey)
        let contentClient = NMPContentClient(engine: engine)
        content = contentClient
        profileSession = contentClient.session(content: "nostr:\(Self.profile)")
        articleSession = contentClient.session(content: "nostr:\(Self.article)")
        noteSession = contentClient.session(content: "nostr:\(Self.note)")
        mixedSession = contentClient.session(
            content: """
            Hello nostr:\(Self.profile) — this sentence is native flow, not an attributed-string trick.

            nostr:\(Self.article)

            The same renderer can quote a normal event below.

            nostr:\(Self.note)
            """
        )
        previewSession = contentClient.session(
            content: "nostr:\(Self.profile) shared a new article"
        )

        let noteDocument = parseNostrContent("nostr:\(Self.note)")
        guard let noteTarget = noteDocument.references.first?.target else {
            throw GallerySeedError.invalidReference("note")
        }
        guard case .event(let noteID, _, _, _) = noteTarget else {
            throw GallerySeedError.invalidReference("note target")
        }
        let unknownRow = Row(
            id: noteID,
            pubkey: Self.profilePubkey,
            createdAt: 1_720_000_000,
            kind: 55_555,
            tags: [],
            content: "An app can replace this made-up kind locally. The standard catalog still gives it honest generic event chrome.",
            sig: String(repeating: "00", count: 64),
            sources: []
        )
        unknownKindSession = .scripted(
            document: noteDocument,
            resources: [
                noteTarget: .resolved(
                    resource: .event(unknownRow),
                    evidence: NostrContentEvidence()
                )
            ]
        )

        let profileDocument = parseNostrContent("nostr:\(Self.profile)")
        guard let profileTarget = profileDocument.references.first?.target else {
            throw GallerySeedError.invalidReference("profile")
        }
        loadingSession = .scripted(
            document: profileDocument,
            resources: [profileTarget: .loading(evidence: NostrContentEvidence())]
        )
        shortfallSession = .scripted(
            document: profileDocument,
            resources: [
                profileTarget: .shortfall(
                    reason: .noPlannedSource,
                    evidence: NostrContentEvidence()
                )
            ]
        )
        cycleSession = .scripted(
            document: noteDocument,
            resources: [
                noteTarget: .collapsed(reason: .cycle(targetKey: noteTarget.key))
            ]
        )
        longContentSession = .scripted(
            content: Self.longMarkdown,
            syntax: .markdown
        )
        stressSessions = (0..<72).map { index in
            contentClient.session(
                content: "Row \(index + 1): nostr:\(Self.profile) quoted nostr:\(Self.note)",
                policy: NostrContentPolicy(
                    maxActiveReferences: 2,
                    maxResolvedReferences: 4,
                    maxDepth: 1,
                    releaseGraceMilliseconds: 0
                )
            )
        }

        Publishers.MergeMany(
            stressSessions.map { session in
                session.$snapshot.map { _ in () }.eraseToAnyPublisher()
            }
        )
        .sink { [weak self] in
            guard let self else { return }
            self.stressActiveReferences = self.stressSessions
                .reduce(0) { $0 + $1.snapshot.activeReferenceCount }
        }
        .store(in: &cancellables)
    }

    deinit {
        engine.shutdown()
    }

    private static let longMarkdown = """
    # Long-form conformance article

    This body proves that long content stays readable at large Dynamic Type sizes. It includes **strong text**, _emphasis_, a #Nostr hashtag, and https://example.com as an ordinary link.

    ## Inline native content

    A profile such as nostr:\(profile) remains a native inline renderer between surrounding words rather than forcing the paragraph into a flat attributed string.

    > A block quote keeps authored structure while the app still owns typography and interaction.

    1. Ordered lists preserve their ordinal.
    2. Wrapped lines grow vertically instead of clipping.
    3. Missing remote resources keep stable placeholders.

    ```swift
    let renderers = NostrContentRenderers.standard
        .event(kind: 55_555) { MyAppOnlyCard(event: $0.event) }
    ```

    The remaining paragraphs deliberately repeat enough prose to exercise scrolling, selection sizing, and accessibility traversal without creating more relay demand. NMP content views remain bounded by visible references, not by the number of words in an article.

    The application can also construct a semantic document for Djot or AsciiDoc and hand it to a scripted or live content session. Parser choice and pixel design remain replaceable while Nostr references keep the same acquisition semantics.

    This final paragraph makes the fixture tall enough to expose any fixed-height assumptions in cards, inline flow, headings, lists, quotes, and code blocks.
    """
}

private enum GallerySeedError: LocalizedError {
    case invalidReference(String)

    var errorDescription: String? {
        switch self {
        case .invalidReference(let label): "Invalid Gallery seed: \(label)"
        }
    }
}
