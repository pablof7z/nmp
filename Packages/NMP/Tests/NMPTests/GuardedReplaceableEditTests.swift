import XCTest
@testable import NMP

/// #597: a native caller can guard a complete, arbitrary replaceable event
/// without a kind-specific helper. These are real engine/store acceptance
/// paths: exact/nil guards sign, while the stale guard never reaches signing.
final class GuardedReplaceableEditTests: XCTestCase {
    private enum Timeout: Error {
        case elapsed
    }

    private let secret = String(repeating: "0", count: 63) + "1"

    private static func collect(_ stream: ReceiptStatus, count: Int) async throws -> [WriteStatus] {
        try await withThrowingTaskGroup(of: [WriteStatus].self) { group in
            group.addTask {
                var statuses: [WriteStatus] = []
                for try await status in stream {
                    statuses.append(status)
                    if statuses.count == count { break }
                }
                return statuses
            }
            group.addTask {
                try await Task.sleep(nanoseconds: 5_000_000_000)
                throw Timeout.elapsed
            }
            guard let statuses = try await group.next() else {
                throw Timeout.elapsed
            }
            group.cancelAll()
            return statuses
        }
    }

    private static func signedID(_ statuses: [WriteStatus]) throws -> String {
        guard statuses.count == 2, statuses[0] == .accepted,
            case .signed(let eventID) = statuses[1]
        else {
            XCTFail("expected Accepted then Signed, got \(statuses)")
            throw Timeout.elapsed
        }
        return eventID
    }

    func testGenericReplaceableGuardExactConflictAndFirstCreation() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }
        let account = try await engine.addAccount(secretKey: secret)
        try engine.setActiveAccount(account.publicKey)

        let base = try await engine.publish(
            WriteIntent(
                payload: .unsigned(
                    pubkey: account.publicKey,
                    createdAt: 1_723_460_000,
                    kind: 10_042,
                    tags: [["x", "caller-owned"]],
                    content: "base"
                ),
                durability: .durable,
                routing: .authorOutbox
            )
        )
        let baseID = try Self.signedID(try await Self.collect(base.status, count: 2))

        let exact = try await engine.publish(
            WriteIntent(
                payload: .unsignedReplaceableEdit(
                    pubkey: account.publicKey,
                    createdAt: 1_723_460_001,
                    kind: 10_042,
                    tags: [["x", "caller-owned"]],
                    content: "exact replacement",
                    expectedBase: baseID
                ),
                durability: .durable,
                routing: .authorOutbox
            )
        )
        let exactID = try Self.signedID(try await Self.collect(exact.status, count: 2))
        XCTAssertNotEqual(exactID, baseID)

        let stale = try await engine.publish(
            WriteIntent(
                payload: .unsignedReplaceableEdit(
                    pubkey: account.publicKey,
                    createdAt: 1_723_460_002,
                    kind: 10_042,
                    tags: [["x", "caller-owned"]],
                    content: "stale replacement",
                    expectedBase: baseID
                ),
                durability: .durable,
                routing: .authorOutbox
            )
        )
        let staleStatuses = try await Self.collect(stale.status, count: 1)
        XCTAssertEqual(
            staleStatuses,
            [.replaceableConflict(expected: baseID, actual: exactID)]
        )
        guard case .notFound = try engine.reattachReceipt(id: stale.id) else {
            return XCTFail("a stale guard must leave no durable receipt")
        }

        let first = try await engine.publish(
            WriteIntent(
                payload: .unsignedReplaceableEdit(
                    pubkey: account.publicKey,
                    createdAt: 1_723_460_003,
                    kind: 10_043,
                    tags: [],
                    content: "first value",
                    expectedBase: nil
                ),
                durability: .durable,
                routing: .authorOutbox
            )
        )
        let firstID = try Self.signedID(try await Self.collect(first.status, count: 2))
        XCTAssertEqual(firstID.count, 64)
    }
}
