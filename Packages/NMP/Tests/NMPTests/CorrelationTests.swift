import XCTest
@testable import NMP

/// #591: crash-safe publish correlation exercised through the public Swift
/// SDK -- a caller-generated token reattaches an existing obligation
/// instead of enqueuing a second write, and `reattachReceipt(correlation:)`
/// recovers a receipt the caller never learned the numeric id of.
final class CorrelationTests: XCTestCase {
    private enum Timeout: Error {
        case elapsed
    }

    private let author = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"

    private static func collect(_ stream: AsyncStream<WriteStatus>, count: Int) async -> [WriteStatus] {
        var statuses: [WriteStatus] = []
        for await status in stream {
            statuses.append(status)
            if statuses.count >= count { break }
        }
        return statuses
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

    func testDoubleSubmitWithTheSameTokenReattachesInsteadOfEnqueuingASecondWrite() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }
        try engine.setActiveAccount(author)

        let token = "swift-sdk-correlation-token"
        let first = try await engine.publish(
            WriteIntent(
                payload: .unsigned(
                    pubkey: author,
                    createdAt: 1_723_456_800,
                    kind: 1,
                    tags: [],
                    content: "first draft"
                ),
                durability: .durable,
                routing: .authorOutbox,
                correlation: token
            )
        )
        let firstStatuses = try await Self.withTimeout {
            await Self.collect(first.status, count: 2)
        }
        XCTAssertEqual(
            firstStatuses,
            [.accepted, .awaitingCapability(pubkey: author)]
        )

        // A re-composed draft -- different timestamp/content -- under the
        // SAME token must resolve to the SAME receipt id, never a new one.
        let second = try await engine.publish(
            WriteIntent(
                payload: .unsigned(
                    pubkey: author,
                    createdAt: 1_723_456_801,
                    kind: 1,
                    tags: [],
                    content: "second, different draft"
                ),
                durability: .durable,
                routing: .authorOutbox,
                correlation: token
            )
        )
        XCTAssertEqual(second.id, first.id)
        let secondStatuses = try await Self.withTimeout {
            await Self.collect(second.status, count: 2)
        }
        XCTAssertEqual(
            secondStatuses,
            [.accepted, .awaitingCapability(pubkey: author)],
            "the retry's stream must replay the ORIGINAL obligation's facts"
        )
    }

    func testReattachByCorrelationRecoversAReceiptTheCallerNeverLearnedTheIdOf() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }
        try engine.setActiveAccount(author)

        let token = "swift-sdk-reattach-by-correlation"
        let receipt = try await engine.publish(
            WriteIntent(
                payload: .unsigned(
                    pubkey: author,
                    createdAt: 1_723_456_900,
                    kind: 1,
                    tags: [],
                    content: "reattach by correlation"
                ),
                durability: .durable,
                routing: .authorOutbox,
                correlation: token
            )
        )
        _ = try await Self.withTimeout {
            await Self.collect(receipt.status, count: 2)
        }

        // Simulate the "app forgot the numeric id" scenario: reattach using
        // only the token it minted itself.
        guard case .attached(let replay) = try engine.reattachReceipt(correlation: token) else {
            return XCTFail("a token that resolved during publish must remain reattachable")
        }
        let replayStatuses = try await Self.withTimeout {
            await Self.collect(replay.status, count: 2)
        }
        XCTAssertEqual(
            replayStatuses,
            [.accepted, .awaitingCapability(pubkey: author)]
        )

        // An unknown token is a distinct, typed absence.
        guard case .notFound = try engine.reattachReceipt(correlation: "never-seen-token") else {
            return XCTFail("an unknown correlation token must report notFound")
        }
    }

    func testMalformedCorrelationTokenOnPublishThrowsSynchronously() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }
        try engine.setActiveAccount(author)

        do {
            _ = try await engine.publish(
                WriteIntent(
                    payload: .unsigned(
                        pubkey: author,
                        createdAt: 1_723_457_000,
                        kind: 1,
                        tags: [],
                        content: "malformed correlation token"
                    ),
                    durability: .durable,
                    routing: .authorOutbox,
                    correlation: ""
                )
            )
            XCTFail("an empty correlation token must be a typed synchronous refusal")
        } catch NMPError.invalidCorrelationToken(let got, _) {
            XCTAssertEqual(got, "")
        }
    }

    func testAnUnknownCorrelationTokenReportsNotFound() throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }

        guard case .notFound = try engine.reattachReceipt(correlation: "never-seen-token") else {
            return XCTFail("an unknown correlation token must report notFound")
        }
    }
}
