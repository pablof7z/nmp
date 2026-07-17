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

    private static func collect(_ stream: AsyncStream<WriteStatus>, count: Int) async -> [WriteStatus] {
        var statuses: [WriteStatus] = []
        for await status in stream {
            statuses.append(status)
            if statuses.count >= count { break }
        }
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
                    ["I", "guid-1"],
                    ["K", "podcast:item:guid"],
                    ["i", "guid-1"],
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
                    ["I", "guid-1"], ["K", "podcast:item:guid"],
                    ["i", "guid-DIFFERENT"], ["k", "podcast:item:guid"],
                ])
            )
        ) { error in
            XCTAssertEqual(error as? CommentDecodeError, .parentDoesNotMatchRootOrComment)
        }

        // Duplicate contradictory root tags.
        XCTAssertThrowsError(
            try decodeComment(
                row(tags: [
                    ["E", String(repeating: "1", count: 64)], ["I", "guid-1"],
                    ["K", "podcast:item:guid"], ["i", "guid-1"], ["k", "podcast:item:guid"],
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
    /// falsifier: compose a comment intent while the active identity has
    /// no signer, publish, observe `Accepted` + `AwaitingCapability`
    /// (the canonical "locally pending" state an app renders without
    /// interpreting `sources`/the all-zero sig sentinel itself), and prove
    /// the SAME token reattaches the identical obligation.
    func testOfflineSignerDurableAcceptanceAndCorrelationReattachment() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }
        try engine.setActiveAccount(author)

        let token = "nip22-offline-signer-token"
        let intent = try engine.commentIntent(
            root: .external(target: .podcastEpisodeGuid(guid: "guid-offline")),
            parent: .root,
            authorPubkey: author,
            createdAt: 1_723_458_000,
            content: "great show",
            correlation: token
        )
        let receipt = try await engine.publishComposed(intent)
        let statuses = try await Self.withTimeout {
            await Self.collect(receipt.status, count: 2)
        }
        XCTAssertEqual(statuses, [.accepted, .awaitingCapability(pubkey: author)])

        // The app never learned the numeric receipt id (it only minted the
        // token) -- reattach using only the token, mirroring a restart.
        guard case .attached(let replay) = try engine.reattachReceipt(correlation: token) else {
            return XCTFail("a token that resolved during publish must remain reattachable")
        }
        let replayStatuses = try await Self.withTimeout {
            await Self.collect(replay.status, count: 2)
        }
        XCTAssertEqual(replayStatuses, [.accepted, .awaitingCapability(pubkey: author)])
    }
}
