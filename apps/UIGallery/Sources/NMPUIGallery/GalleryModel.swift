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
    let content: NMPContentClient
    let profileSession: NostrContentSession
    let articleSession: NostrContentSession
    let noteSession: NostrContentSession
    let mixedSession: NostrContentSession
    let previewSession: NostrContentSession

    init() throws {
        let caches = FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask).first
        let store = caches?.appendingPathComponent("nmp-ui-gallery.redb").path
        engine = try NMPEngine(
            config: NMPConfig(storePath: store, indexerRelays: Self.indexers)
        )
        content = NMPContentClient(engine: engine)
        profileSession = content.session(content: "nostr:\(Self.profile)")
        articleSession = content.session(content: "nostr:\(Self.article)")
        noteSession = content.session(content: "nostr:\(Self.note)")
        mixedSession = content.session(
            content: """
            Hello nostr:\(Self.profile) — this sentence is native flow, not an attributed-string trick.

            nostr:\(Self.article)

            The same renderer can quote a normal event below.

            nostr:\(Self.note)
            """
        )
        previewSession = content.session(
            content: "nostr:\(Self.profile) shared a new article"
        )
    }

    deinit {
        engine.shutdown()
    }
}
