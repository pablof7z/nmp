import Combine
import Foundation
import NMP
import NMPContent
import NMPUI

@MainActor
final class GalleryModel: ObservableObject {
    static let indexers = ["wss://purplepag.es", "wss://relay.primal.net"]
    static let profile = "nprofile1qqsrhuxx8l9ex335q7he0f09aej04zpazpl0ne2cgukyawd24mayt8gpzfmhxue69uhhqatjwpkx2urpvuhx2ucl9q7qz"
    static let article = "naddr1qq3xummnw3ez6atw94shqupdv9kz6emfdaexumedwa5xjar994hx76tnv5pzpqlkerf45wg3uht9d22ym7hdj9xlnklpryzk5px0hd8nc8xu4j6aqvzqqqr4guzrk365"
    static let note = "nevent1qqsvgksrcarvjcltp7a20s3ux9d626zdj8xsdmyduqpz8dudvldl25gzyqalp33lewf5vdq847t6te0wvnags0gs0mu72kz8938tn24wlfze6fh6mfe"
    static let profilePubkey = "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d"

    let engine: NMPEngine
    let following: NMPFollowing
    let observationFactory: NMPReferenceObservationFactory
    let profileDocument: NostrContentDocument
    let articleDocument: NostrContentDocument
    let noteDocument: NostrContentDocument
    let mixedDocument: NostrContentDocument
    let previewDocument: NostrContentDocument
    let longContentDocument: NostrContentDocument
    let stressDocuments: [NostrContentDocument]

    init() throws {
        let caches = FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask).first
        let store = caches?.appendingPathComponent("nmp-ui-gallery-v7.redb").path
        engine = try NMPEngine(
            config: NMPConfig(storePath: store, indexerRelays: Self.indexers)
        )
        following = try NMPFollowing(engine: engine, target: Self.profilePubkey)
        observationFactory = .live(engine: engine)
        profileDocument = parseNostrContent("nostr:\(Self.profile)")
        articleDocument = parseNostrContent("nostr:\(Self.article)")
        noteDocument = parseNostrContent("nostr:\(Self.note)")
        previewDocument = parseNostrContent("nostr:\(Self.profile) shared a new event")
        mixedDocument = parseNostrContent(
            """
            Hello nostr:\(Self.profile) — the selected mention component owns its observation.

            nostr:\(Self.article)

            nostr:\(Self.note)
            """
        )
        longContentDocument = parseNostrContent(Self.longMarkdown, syntax: .markdown)
        stressDocuments = (0..<72).map { index in
            parseNostrContent(
                "Row \(index + 1): nostr:\(Self.profile) linked nostr:\(Self.note)"
            )
        }
    }

    deinit {
        engine.shutdown()
    }

    private static let longMarkdown = """
    # Component-owned reference observations

    Parsing this document is pure. A literal profile such as nostr:\(profile) can remain authored text with zero observations, while another call site may select the standard mention and observe kind:0.

    ## Event loading is a component

    The outer loader for nostr:\(note) decides whether to acquire. Only after a row arrives does the independent table dispatch its actual kind.

    > Cycle and depth state is an immutable ancestor path threaded into nested rendering; there is no mutable document session or count coordinator.
    """
}
