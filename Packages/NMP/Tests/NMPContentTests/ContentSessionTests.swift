import NMP
import NMPContent
import XCTest

@MainActor
final class ContentSessionTests: XCTestCase {
    private let fiatjafNpub = "npub180cvv07tjdrrgpa0j7j7tmnyl2yr6yr7l8j4s3evf6u64th6gkwsyjh6w6"
    private let secondNpub = "npub14f8usejl26twx0dhuxjh9cas7keav9vr0v8nvtwtrjqx3vycc76qqh9nsy"
    /// Intentionally contains no relay TLVs. The author has a kind:10002 on
    /// the configured indexers, so normal NMP outbox discovery must find the
    /// article's relay rather than this test naming one.
    private let noHintArticle =
        "naddr1qq3xummnw3ez6atw94shqupdv9kz6emfdaexumedwa5xjar994hx76tnv5pzpqlkerf45wg3uht9d22ym7hdj9xlnklpryzk5px0hd8nc8xu4j6aqvzqqqr4guzrk365"

    func testDuplicateOccurrencesShareOneActiveTargetAndLastReleaseWithdrawsIt() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }
        let client = NMPContentClient(engine: engine)
        let session = client.session(
            content: "nostr:\(fiatjafNpub) and again nostr:\(fiatjafNpub)",
            policy: NostrContentPolicy(releaseGraceMilliseconds: 0)
        )
        XCTAssertEqual(session.snapshot.document.references.count, 2)

        let first = try XCTUnwrap(session.claim(
            referenceID: session.snapshot.document.references[0].id
        ))
        let second = try XCTUnwrap(session.claim(
            referenceID: session.snapshot.document.references[1].id
        ))
        XCTAssertEqual(session.snapshot.activeReferenceCount, 1)

        first.cancel()
        await Task.yield()
        XCTAssertEqual(session.snapshot.activeReferenceCount, 1)

        second.cancel()
        _ = await eventually { session.snapshot.activeReferenceCount == 0 }
        XCTAssertEqual(session.snapshot.activeReferenceCount, 0)
    }

    func testActiveBudgetSurfacesAndThenAdmitsWaitingClaim() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }
        let client = NMPContentClient(engine: engine)
        let session = client.session(
            content: "nostr:\(fiatjafNpub) nostr:\(secondNpub)",
            policy: NostrContentPolicy(
                maxActiveReferences: 1,
                releaseGraceMilliseconds: 0
            )
        )
        let references = session.snapshot.document.references
        let first = try XCTUnwrap(session.claim(referenceID: references[0].id))
        let second = try XCTUnwrap(session.claim(referenceID: references[1].id))

        XCTAssertEqual(session.snapshot.activeReferenceCount, 1)
        guard case .collapsed(reason: .activeBudget(maximum: 1)) =
            session.snapshot.state(for: references[1])
        else {
            return XCTFail("second target must surface the active-reference bound")
        }

        first.cancel()
        let admitted = await eventually {
            if case .collapsed = session.snapshot.state(for: references[1]) { return false }
            return session.snapshot.activeReferenceCount == 1
        }
        guard admitted else {
            second.cancel()
            return XCTFail("waiting target should start when the first releases")
        }
        second.cancel()
    }

    func testRealProfileReferenceResolvesThroughIndexerOnlyEngine() async throws {
        let engine = try NMPEngine(
            config: NMPConfig(
                indexerRelays: ["wss://purplepag.es", "wss://relay.primal.net"]
            )
        )
        defer { engine.shutdown() }
        let session = NMPContentClient(engine: engine).session(
            content: "hello nostr:\(fiatjafNpub)",
            policy: NostrContentPolicy(releaseGraceMilliseconds: 0)
        )
        let occurrence = try XCTUnwrap(session.snapshot.document.references.first)
        let claim = try XCTUnwrap(session.claim(referenceID: occurrence.id))
        defer { claim.cancel() }

        let resolved = await eventually(timeoutSeconds: 20) {
            session.snapshot.state(for: occurrence).resource?.profile != nil
        }
        guard resolved,
              let profile = session.snapshot.state(for: occurrence).resource?.profile
        else {
            throw XCTSkip(
                "fiatjaf kind:0 did not resolve within 20s from the two indexers; "
                    + "the network may be unavailable in this test environment"
            )
        }

        XCTAssertEqual(
            profile.pubkey,
            "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d"
        )
        XCTAssertTrue(profile.displayName != nil || profile.name != nil)
    }

    func testNoHintAddressResolvesThroughNormalOutboxDiscovery() async throws {
        let engine = try NMPEngine(
            config: NMPConfig(
                indexerRelays: ["wss://purplepag.es", "wss://relay.primal.net"]
            )
        )
        defer { engine.shutdown() }
        let session = NMPContentClient(engine: engine).session(
            content: "read nostr:\(noHintArticle)",
            policy: NostrContentPolicy(releaseGraceMilliseconds: 0)
        )
        let occurrence = try XCTUnwrap(session.snapshot.document.references.first)
        guard case .address(_, _, _, let relayHints) = occurrence.target else {
            return XCTFail("fixture must parse as an address target")
        }
        XCTAssertTrue(relayHints.isEmpty, "the proof must not smuggle a relay in the naddr")
        let claim = try XCTUnwrap(session.claim(referenceID: occurrence.id))
        defer { claim.cancel() }

        let resolved = await eventually(timeoutSeconds: 30) {
            session.snapshot.state(for: occurrence).resource?.article != nil
        }
        guard resolved,
              let resource = session.snapshot.state(for: occurrence).resource,
              let article = resource.article
        else {
            throw XCTSkip(
                "the no-hint article did not resolve within 30s; its discovered outbox relay "
                    + "may be unavailable in this test environment"
            )
        }

        XCTAssertEqual(resource.event.kind, 30_023)
        XCTAssertEqual(article.identifier, "nostr-un-app-al-giorno-white-noise")
        XCTAssertEqual(article.title, "Nostr, un app al giorno: White Noise")
        XCTAssertFalse(resource.event.sources.isEmpty)
    }

    private func eventually(
        timeoutSeconds: UInt64 = 2,
        condition: @escaping @MainActor () -> Bool
    ) async -> Bool {
        let attempts = Int(timeoutSeconds * 100)
        for _ in 0..<attempts {
            if condition() { return true }
            try? await Task.sleep(nanoseconds: 10_000_000)
        }
        return condition()
    }
}
