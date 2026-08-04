import XCTest
@testable import NMP
import NMPFFI

final class NIP22Tests: XCTestCase {
    private enum Timeout: Error {
        case elapsed
    }

    private static func withTimeout<T: Sendable>(
        _ operation: @escaping @Sendable () async throws -> T
    ) async throws -> T {
        try await withThrowingTaskGroup(of: T.self) { group in
            group.addTask { try await operation() }
            group.addTask {
                try await Task.sleep(nanoseconds: 5_000_000_000)
                throw Timeout.elapsed
            }
            guard let result = try await group.next() else {
                throw Timeout.elapsed
            }
            group.cancelAll()
            return result
        }
    }

    private static func collect(_ stream: ReceiptStatus, count: Int) async -> [WriteFact] {
        var statuses: [WriteFact] = []
        // #680: a receipt is a throwing `AsyncSequence`; a throw here is
        // terminal teardown, so end collection with what we have.
        do {
            for try await status in stream {
                statuses.append(status)
                if statuses.count >= count { break }
            }
        } catch {}
        return statuses
    }

    private let author = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"

    /// Root-thread demand scopes kind:1111 by the uppercase `#I` tag --
    /// never a parent-only lowercase `#i` shortcut.
    func testCommentThreadDemandScopesKind1111ByUppercaseITag() throws {
        let root = CommentRoot.external(target: .podcastEpisodeGuid(guid: "guid-1"))
        let demand = try commentThreadDemand(root: root)
        XCTAssertEqual(demand.selection.kinds, [1111])
    }

    /// Mirrors the NIP-29 kind:10009 decode-fixture pattern (#108): build a
    /// fixture `Row` with the exact required tags and assert the decoded
    /// typed value.
    func testDecodeCommentComposesATypedTopLevelPodcastComment() {
        let row = Row(
            FfiRow(
                id: String(repeating: "1", count: 64), pubkey: author, createdAt: 1, kind: 1111,
                tags: [
                    ["I", "podcast:item:guid:guid-1"],
                    ["K", "podcast:item:guid"],
                    ["i", "podcast:item:guid:guid-1"],
                    ["k", "podcast:item:guid"],
                ],
                content: "nice episode", sig: String(repeating: "0", count: 128), sources: []
            )
        )
        let decoded = try! decodeComment(row)
        XCTAssertEqual(decoded.root, .external(target: .podcastEpisodeGuid(guid: "guid-1")))
        XCTAssertEqual(decoded.parent, .root)
        XCTAssertEqual(decoded.content, "nice episode")
    }

    /// #572 review finding 1: a `K == podcast:item:guid` cell whose `I`
    /// value is the BARE guid (missing NIP-73's required
    /// `podcast:item:guid:` prefix) is a typed refusal, never silently
    /// accepted -- a bare-guid comment would split the episode's thread
    /// from conformant clients (e.g. Fountain).
    func testPodcastGuidMissingPrefixIsRejected() {
        let row = Row(
            FfiRow(
                id: String(repeating: "1", count: 64), pubkey: author, createdAt: 1, kind: 1111,
                tags: [
                    ["I", "guid-1"],
                    ["K", "podcast:item:guid"],
                    ["i", "guid-1"],
                    ["k", "podcast:item:guid"],
                ],
                content: "", sig: String(repeating: "0", count: 128), sources: []
            )
        )
        XCTAssertThrowsError(try decodeComment(row)) { error in
            XCTAssertEqual(error as? CommentDecodeError, .malformedExternalValue(got: "guid-1"))
        }
    }

    /// Missing K/k, mismatched I/i, and duplicate contradictory root tags
    /// never become a typed comment -- the malformed-rejection matrix.
    func testMalformedTagSetsAreRejectedNotSilentlyCoerced() {
        func row(tags: [[String]]) -> Row {
            Row(
                FfiRow(
                    id: String(repeating: "1", count: 64), pubkey: author, createdAt: 1, kind: 1111,
                    tags: tags, content: "", sig: String(repeating: "0", count: 128), sources: []
                )
            )
        }

        // Missing K.
        XCTAssertThrowsError(try decodeComment(row(tags: [["I", "guid-1"], ["i", "guid-1"], ["k", "podcast:item:guid"]]))) { error in
            XCTAssertEqual(error as? CommentDecodeError, .missingRootKind)
        }

        // Missing k.
        XCTAssertThrowsError(try decodeComment(row(tags: [["I", "guid-1"], ["K", "podcast:item:guid"], ["i", "guid-1"]]))) { error in
            XCTAssertEqual(error as? CommentDecodeError, .missingParentKind)
        }

        // Mismatched I/i.
        XCTAssertThrowsError(
            try decodeComment(
                row(tags: [
                    ["I", "podcast:item:guid:guid-1"], ["K", "podcast:item:guid"],
                    ["i", "podcast:item:guid:guid-DIFFERENT"], ["k", "podcast:item:guid"],
                ])
            )
        ) { error in
            XCTAssertEqual(error as? CommentDecodeError, .parentDoesNotMatchRootOrComment)
        }

        // Duplicate contradictory root tags (different letters: E and I).
        XCTAssertThrowsError(
            try decodeComment(
                row(tags: [
                    ["E", String(repeating: "1", count: 64)], ["I", "podcast:item:guid:guid-1"],
                    ["K", "podcast:item:guid"], ["i", "podcast:item:guid:guid-1"],
                    ["k", "podcast:item:guid"],
                ])
            )
        ) { error in
            XCTAssertEqual(error as? CommentDecodeError, .duplicateContradictoryRoot)
        }

        // Duplicate SAME-letter root tags (two contradictory I tags) --
        // #572 review finding 3: same-letter duplicates must not silently
        // resolve to "first one wins".
        XCTAssertThrowsError(
            try decodeComment(
                row(tags: [
                    ["I", "podcast:item:guid:guid-1"], ["I", "podcast:item:guid:guid-2"],
                    ["K", "podcast:item:guid"], ["i", "podcast:item:guid:guid-1"],
                    ["k", "podcast:item:guid"],
                ])
            )
        ) { error in
            XCTAssertEqual(error as? CommentDecodeError, .duplicateContradictoryRoot)
        }

        // An unrelated event with no NIP-22 tags at all.
        XCTAssertThrowsError(try decodeComment(row(tags: [["t", "podcast"]]))) { error in
            XCTAssertEqual(error as? CommentDecodeError, .missingRoot)
        }
    }

    /// #572's offline-signer durable acceptance + restart reattachment
    /// falsifier: compose a comment intent while the active identity has no
    /// signer, publish -- whose returning receipt IS the durable acceptance --
    /// observe the parked `SigningState.awaitingSigner` (the canonical
    /// "locally pending" state an app renders without interpreting
    /// `sources`/the all-zero sig sentinel itself), and prove the SAME token
    /// reattaches the identical obligation.
    func testOfflineSignerDurableAcceptanceAndCorrelationReattachment() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }
        try engine.setActiveAccount(author)

        let token = "nip22-offline-signer-token"
        let intent = try NMP.commentIntent(
            root: .external(target: .podcastEpisodeGuid(guid: "guid-offline")),
            parent: .root,
            content: "great show",
            correlation: token
        )
        let receipt = try await engine.publish(intent)
        let statuses = try await Self.withTimeout {
            await Self.collect(receipt.status, count: 1)
        }
        XCTAssertEqual(statuses, [.signing(.awaitingSigner(pubkey: author))])

        // The app never learned the numeric receipt id (it only minted the
        // token) -- reattach using only the token, mirroring a restart.
        guard case .attached(let replay) = try engine.reattachReceipt(correlation: token) else {
            return XCTFail("a token that resolved during publish must remain reattachable")
        }
        let replayStatuses = try await Self.withTimeout {
            await Self.collect(replay.status, count: 1)
        }
        XCTAssertEqual(replayStatuses, [.signing(.awaitingSigner(pubkey: author))])
    }

    // MARK: - #572 review finding 4: test honesty

    /// A "golden fixture" once lived here: a fixed key, timestamp and
    /// content whose composed event id and exact NIP-01 JSON were asserted
    /// identical in Rust, Swift and Kotlin. It is gone, deliberately. The
    /// composer no longer produces bytes -- it produces a schema, and the
    /// author and the timestamp that complete those bytes are decided at
    /// acceptance. What all three languages still assert is the thing NIP-22
    /// actually owns: the exact tag rows, in the exact order.
    func testComposedCommentPinsTheExactTagRows() throws {
        let intent = try NMP.commentIntent(
            root: .external(target: .podcastEpisodeGuid(guid: "golden-guid-572")),
            parent: .root,
            content: "golden fixture content"
        )
        guard case .event(let kind, let tags, let content, let createdAt) = intent.payload else {
            return XCTFail("NIP-22 must compose an ordinary builder write payload")
        }
        XCTAssertEqual(kind, 1111)
        XCTAssertEqual(content, "golden fixture content")
        XCTAssertNil(createdAt, "acceptance stamps it; the composer never invents one")
        XCTAssertEqual(
            tags,
            [
                ["I", "podcast:item:guid:golden-guid-572"],
                ["K", "podcast:item:guid"],
                ["i", "podcast:item:guid:golden-guid-572"],
                ["k", "podcast:item:guid"],
            ]
        )
        XCTAssertEqual(intent.routing, .auto)
        XCTAssertEqual(intent.identity, .active)
        XCTAssertNil(intent.correlation)
    }

    func testDurableAcceptanceMakesOneCanonicalPendingCommentVisibleThroughTheQueryPath() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }
        try engine.setActiveAccount(author)

        let root = CommentRoot.external(target: .podcastEpisodeGuid(guid: "guid-query-path"))
        let demand = try commentThreadDemand(root: root)
        let query = try engine.observe(demand)

        let intent = try NMP.commentIntent(
            root: root,
            parent: .root,
            content: "visible through the ordinary query path"
        )
        let receipt = try await engine.publish(intent)
        _ = try await Self.withTimeout {
            await Self.collect(receipt.status, count: 1)
        }

        let row = try await Self.withTimeout {
            await Self.firstRow(from: query, timeoutSeconds: 5)
        }
        guard let row else {
            return XCTFail("the durably-accepted pending comment must be visible through observe(demand)")
        }
        XCTAssertEqual(row.pubkey, author)
        XCTAssertEqual(row.kind, 1111)

        let decoded = try decodeComment(row)
        XCTAssertEqual(decoded.root, root)
        XCTAssertEqual(decoded.parent, .root)
        XCTAssertEqual(decoded.content, "visible through the ordinary query path")
    }

    private static func firstRow(from query: NMPQuery, timeoutSeconds: UInt64) async -> Row? {
        await withTaskGroup(of: Row?.self) { group in
            group.addTask {
                do {
                    for try await batch in query {
                        if let row = batch.rows.first {
                            return row
                        }
                    }
                } catch {}
                return nil
            }
            group.addTask {
                try? await Task.sleep(nanoseconds: timeoutSeconds * 1_000_000_000)
                return nil
            }
            let result = await group.next() ?? nil
            group.cancelAll()
            return result
        }
    }
}
