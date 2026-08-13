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
        let engine = try NMPEngine(
            config: NMPConfig(
                outboxRouting: OutboxRoutingConfig(indexers: ["wss://indexer.example"])
            )
        )
        defer { engine.shutdown() }
        _ = try engine.session.add(publicKey: testPublicKey(author), makeCurrent: true)

        let token = "swift-sdk-correlation-token"
        let first = try await engine.publish(
            WriteIntent(
                payload: .event(
                    kind: 1,
                    tags: [],
                    content: "first draft",
                    createdAt: 1_723_456_800
                ),
                routing: .auto,
                correlation: token
            )
        )
        // `publish` returning `first` IS the acceptance; the stream carries
        // only what happened AFTER it, and with no signer registered for the
        // current account that is the parked signing obligation.
        let firstStatuses = try await Self.withTimeout {
            await Self.collect(first.status, count: 1)
        }
        XCTAssertEqual(
            firstStatuses,
            [.signing(.awaitingSigner(pubkey: author))]
        )

        // A re-composed draft -- different timestamp/content -- under the
        // SAME token must resolve to the SAME receipt id, never a new one.
        let second = try await engine.publish(
            WriteIntent(
                payload: .event(
                    kind: 1,
                    tags: [],
                    content: "second, different draft",
                    createdAt: 1_723_456_801
                ),
                routing: .auto,
                correlation: token
            )
        )
        XCTAssertEqual(second.id, first.id)
        let secondStatuses = try await Self.withTimeout {
            await Self.collect(second.status, count: 1)
        }
        XCTAssertEqual(
            secondStatuses,
            [.signing(.awaitingSigner(pubkey: author))],
            "the retry's stream must replay the ORIGINAL obligation's facts"
        )
    }

    func testReattachByCorrelationRecoversAReceiptTheCallerNeverLearnedTheIdOf() async throws {
        let engine = try NMPEngine(
            config: NMPConfig(
                outboxRouting: OutboxRoutingConfig(indexers: ["wss://indexer.example"])
            )
        )
        defer { engine.shutdown() }
        _ = try engine.session.add(publicKey: testPublicKey(author), makeCurrent: true)

        let token = "swift-sdk-reattach-by-correlation"
        let receipt = try await engine.publish(
            WriteIntent(
                payload: .event(
                    kind: 1,
                    tags: [],
                    content: "reattach by correlation",
                    createdAt: 1_723_456_900
                ),
                routing: .auto,
                correlation: token
            )
        )
        _ = try await Self.withTimeout {
            await Self.collect(receipt.status, count: 1)
        }

        // Simulate the "app forgot the numeric id" scenario: reattach using
        // only the token it minted itself.
        guard case .attached(let replay) = try engine.reattachReceipt(correlation: token) else {
            return XCTFail("a token that resolved during publish must remain reattachable")
        }
        let replayStatuses = try await Self.withTimeout {
            await Self.collect(replay.status, count: 1)
        }
        XCTAssertEqual(
            replayStatuses,
            [.signing(.awaitingSigner(pubkey: author))]
        )

        // An unknown token is a distinct, typed absence.
        guard case .notFound = try engine.reattachReceipt(correlation: "never-seen-token") else {
            return XCTFail("an unknown correlation token must report notFound")
        }
    }

    func testMalformedCorrelationTokenOnPublishThrowsSynchronously() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }
        _ = try engine.session.add(publicKey: testPublicKey(author), makeCurrent: true)

        do {
            _ = try await engine.publish(
                WriteIntent(
                    payload: .event(
                        kind: 1,
                        tags: [],
                        content: "malformed correlation token",
                        createdAt: 1_723_457_000
                    ),
                        routing: .auto,
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
