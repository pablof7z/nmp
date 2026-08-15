import XCTest
@testable import NMP

final class FollowingTests: XCTestCase {
    private static let target = String(repeating: "ab", count: 32)

    func testSignedOutObservationIsUnknownAndUnavailable() async throws {
        let engine = try NMPEngine(config: Self.config)
        defer { engine.shutdown() }

        let observation = try engine.observeFollowing(Self.target)
        guard let snapshot = await Self.firstSnapshot(from: observation) else {
            return XCTFail("NMP must project the signed-out state without relay I/O")
        }

        XCTAssertNil(snapshot.currentPubkey)
        XCTAssertEqual(snapshot.target, Self.target)
        XCTAssertEqual(snapshot.relationship, .unknown)
        XCTAssertEqual(snapshot.availability, .signedOut)
        XCTAssertNil(snapshot.baseEventID)
    }

    /// #1640: a signed-out follow is a truthful immediate refusal -- there is
    /// no receipt, and therefore no stream, to observe it through.
    func testSignedOutFollowRefusesBeforeReceiptCustody() throws {
        let engine = try NMPEngine(config: Self.config)
        defer { engine.shutdown() }

        XCTAssertThrowsError(try engine.follow(Self.target)) { error in
            XCTAssertEqual(error as? FollowActionError, .signedOut)
        }
    }

    /// #1640: an unparseable target refuses synchronously, exactly like every
    /// other pre-custody refusal -- there is no separate typed-action-state
    /// channel for it to hide in.
    func testInvalidTargetRefusesBeforeReceiptCustody() throws {
        let engine = try NMPEngine(config: Self.config)
        defer { engine.shutdown() }

        XCTAssertThrowsError(try engine.follow("not-a-pubkey")) { error in
            XCTAssertEqual(error as? FollowActionError, .invalidTarget(got: "not-a-pubkey"))
        }
    }

    func testProviderlessFollowRefusesBeforeReceiptCustody() throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }

        XCTAssertThrowsError(try engine.follow(Self.target)) { error in
            XCTAssertEqual(error as? FollowActionError, .automaticRoutingUnavailable)
        }
    }

    private static func firstSnapshot(
        from observation: NMPFollowingObservation
    ) async -> NMPFollowingSnapshot? {
        await withTaskGroup(of: NMPFollowingSnapshot?.self) { group in
            group.addTask {
                var iterator = observation.makeAsyncIterator()
                return (try? await iterator.next()) ?? nil
            }
            group.addTask {
                try? await Task.sleep(nanoseconds: 3_000_000_000)
                return nil
            }
            let result = await group.next() ?? nil
            observation.cancel()
            group.cancelAll()
            return result
        }
    }

    private static var config: NMPConfig {
        NMPConfig(outboxRouting: OutboxRoutingConfig(indexers: ["wss://indexer.example"]))
    }
}
