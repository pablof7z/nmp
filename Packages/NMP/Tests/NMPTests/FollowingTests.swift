import XCTest
@testable import NMP

final class FollowingTests: XCTestCase {
    private static let target = String(repeating: "ab", count: 32)

    func testSignedOutObservationIsUnknownAndUnavailable() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }

        let observation = try engine.observeFollowing(Self.target)
        guard let snapshot = await Self.firstSnapshot(from: observation) else {
            return XCTFail("NMP must project the signed-out state without relay I/O")
        }

        XCTAssertNil(snapshot.activePubkey)
        XCTAssertEqual(snapshot.target, Self.target)
        XCTAssertEqual(snapshot.relationship, .unknown)
        XCTAssertEqual(snapshot.availability, .signedOut)
        XCTAssertNil(snapshot.baseEventID)
    }

    func testFollowIsAnNMPActionWithTypedSignedOutFailure() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }

        let action = engine.follow(Self.target)
        let statuses = await Self.firstStatuses(from: action, count: 2)

        XCTAssertEqual(statuses, [.acquiring, .failed(.signedOut)])
    }

    func testInvalidTargetIsTypedActionStateNotANativeException() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }

        let action = engine.follow("not-a-pubkey")
        let statuses = await Self.firstStatuses(from: action, count: 1)
        XCTAssertEqual(statuses, [.failed(.invalidTarget("not-a-pubkey"))])
    }

    private static func firstSnapshot(
        from observation: NMPFollowingObservation
    ) async -> NMPFollowingSnapshot? {
        await withTaskGroup(of: NMPFollowingSnapshot?.self) { group in
            group.addTask {
                var iterator = observation.makeAsyncIterator()
                return await iterator.next()
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

    private static func firstStatuses(
        from action: NMPFollowAction,
        count: Int
    ) async -> [NMPFollowActionStatus] {
        await withTaskGroup(of: [NMPFollowActionStatus].self) { group in
            group.addTask {
                var result: [NMPFollowActionStatus] = []
                for await status in action.status {
                    result.append(status)
                    if result.count == count { break }
                }
                return result
            }
            group.addTask {
                try? await Task.sleep(nanoseconds: 3_000_000_000)
                return []
            }
            let result = await group.next() ?? []
            group.cancelAll()
            return result
        }
    }
}
