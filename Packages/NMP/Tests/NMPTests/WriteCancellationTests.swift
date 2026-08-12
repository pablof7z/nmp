import XCTest
@testable import NMP

final class WriteCancellationTests: XCTestCase {
    private enum Timeout: Error {
        case elapsed
    }

    private let author = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"

    private static func collect(_ stream: ReceiptStatus) async -> [WriteFact] {
        var statuses: [WriteFact] = []
        do {
            for try await status in stream {
                statuses.append(status)
            }
        } catch {
            // The receipt stream ended; return whatever durable prefix arrived.
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

    func testAcceptedUnsignedWriteCancelsStreamsAndReattachesAsCancelled() async throws {
        let engine = try NMPEngine(
            config: NMPConfig(
                nip65: NIP65Config(indexerRelays: ["wss://indexer.example"])
            )
        )
        defer { engine.shutdown() }

        // A read-only active identity deliberately has no signer. The
        // unsigned write therefore remains accepted and cancellable until
        // the explicit receipt-id transition below.
        try engine.setActiveAccount(author)
        let receipt = try await engine.publish(
            WriteIntent(
                payload: .event(
                    kind: 1,
                    tags: [],
                    content: "cancel through the public Swift SDK",
                    createdAt: 1_723_456_790
                ),
                routing: .auto
            )
        )

        // `publish` returning a receipt IS acceptance -- there is no
        // acceptance fact on the stream to look for, and the id it hands back
        // is what the cancellation below is addressed to.
        XCTAssertEqual(try engine.cancel(receiptId: receipt.id), .cancelled)
        let statuses = try await Self.withTimeout {
            await Self.collect(receipt.status)
        }
        XCTAssertFalse(statuses.contains { status in
            if case .signing(.signed) = status { return true }
            return false
        })
        XCTAssertEqual(statuses.last, .outcome(.notSent(.cancelled)))

        // The cancellation transition is idempotent and the durable terminal
        // fact is independently reconstructible by id.
        XCTAssertEqual(try engine.cancel(receiptId: receipt.id), .cancelled)
        guard case .attached(let replay) = try engine.reattachReceipt(id: receipt.id) else {
            return XCTFail("cancelled receipt must remain reattachable")
        }
        let replayStatuses = try await Self.withTimeout {
            await Self.collect(replay.status)
        }
        XCTAssertEqual(replayStatuses, [.outcome(.notSent(.cancelled))])
    }
}
