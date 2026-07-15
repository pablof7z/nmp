import NMPFFI
import XCTest
@testable import NMP

final class HistoryTests: XCTestCase {
    private static let emptyEvidence = FfiAcquisitionEvidence(sources: [], shortfall: [])

    func testBridgeConsumesExactDeltasButRetainsAuthoritativeBoundedRows() async throws {
        var continuation: AsyncStream<HistoryBatch>.Continuation!
        let stream = AsyncStream<HistoryBatch> { continuation = $0 }
        let bridge = HistoryBridge(continuation: continuation, maxRows: 2)
        let a = ffiRow(id: "a", createdAt: 300)
        let b = ffiRow(id: "b", createdAt: 200)
        let c = ffiRow(id: "c", createdAt: 400)

        bridge.onBatch(
            batch: FfiHistoryBatch(
                rows: [a],
                deltas: [.added(row: a)],
                continuation: nil,
                evidence: Self.emptyEvidence,
                load: .idle
            )
        )
        bridge.onBatch(
            batch: FfiHistoryBatch(
                rows: [a, b],
                deltas: [.added(row: b)],
                continuation: nil,
                evidence: Self.emptyEvidence,
                load: .returned(added: 1)
            )
        )
        bridge.onBatch(
            batch: FfiHistoryBatch(
                rows: [c, a],
                deltas: [.removed(id: "b"), .added(row: c)],
                continuation: nil,
                evidence: Self.emptyEvidence,
                load: .requesting
            )
        )
        bridge.onClosed()

        var delivered: [HistoryBatch] = []
        for await batch in stream {
            delivered.append(batch)
        }
        XCTAssertFalse(delivered.isEmpty)
        XCTAssertTrue(delivered.allSatisfy { $0.rows.count <= 2 })
        XCTAssertEqual(delivered.last?.rows.map(\.id), ["c", "a"])
        XCTAssertEqual(delivered.last?.load, .requesting)
    }

    func testHistoryQueryValidationAndCancellationReturnNativeTaskBaseline() async throws {
        let engine = try NMPEngine(config: NMPConfig(maxNativeTasks: 1))
        defer { engine.shutdown() }
        let demand = NMPDemand(
            selection: NMPFilter(kinds: [7_779]),
            source: .public
        )

        XCTAssertThrowsError(
            try engine.observeHistory(demand, pageSize: 0, maxRows: 2)
        ) { error in
            XCTAssertEqual(error as? NMPError, .historyZeroPageSize)
        }
        XCTAssertThrowsError(
            try engine.observeHistory(demand, pageSize: 3, maxRows: 2)
        ) { error in
            XCTAssertEqual(
                error as? NMPError,
                .historyPageExceedsMaxRows(pageSize: 3, maxRows: 2)
            )
        }
        let limited = NMPDemand(
            selection: NMPFilter(kinds: [7_779], limit: 1),
            source: .public
        )
        XCTAssertThrowsError(
            try engine.observeHistory(limited, pageSize: 1, maxRows: 2)
        ) { error in
            XCTAssertEqual(error as? NMPError, .historySelectionHasLimit)
        }

        let query = try engine.observeHistory(demand, pageSize: 1, maxRows: 2)
        XCTAssertEqual(engine.nativeTaskCensus().admitted, 1)
        let first = await Self.firstBatch(from: query, timeoutSeconds: 5)
        XCTAssertEqual(first?.rows, [])
        XCTAssertEqual(first?.load, .idle)
        XCTAssertNil(first?.continuation)

        query.cancel()
        engine.awaitNativeTasksIdle()
        XCTAssertEqual(engine.nativeTaskCensus().admitted, 0)
        XCTAssertEqual(engine.nativeTaskCensus().running, 0)
    }

    func testEngineShutdownClosesAHistoryIteratorWithinBound() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        let demand = NMPDemand(
            selection: NMPFilter(kinds: [7_780]),
            source: .public
        )
        let query = try engine.observeHistory(demand, pageSize: 1, maxRows: 2)
        _ = await Self.firstBatch(from: query, timeoutSeconds: 5)

        let closed = Task {
            var iterator = query.makeAsyncIterator()
            return await iterator.next() == nil
        }
        engine.shutdown()
        let didClose = await Self.value(of: closed, timeoutSeconds: 5) ?? false
        XCTAssertTrue(didClose)
    }

    func testEveryHistoryLoadFailureKeepsItsTypedAxis() {
        XCTAssertEqual(NMPHistoryLoadError(.WrongVersion), .wrongVersion)
        XCTAssertEqual(NMPHistoryLoadError(.WrongEngine), .wrongEngine)
        XCTAssertEqual(NMPHistoryLoadError(.WrongSession), .wrongSession)
        XCTAssertEqual(NMPHistoryLoadError(.WrongDescriptor), .wrongDescriptor)
        XCTAssertEqual(NMPHistoryLoadError(.StaleGeneration), .staleGeneration)
        XCTAssertEqual(NMPHistoryLoadError(.LoadInProgress), .loadInProgress)
        XCTAssertEqual(NMPHistoryLoadError(.AtBound(maxRows: 2)), .atBound(maxRows: 2))
        XCTAssertEqual(NMPHistoryLoadError(.NoBoundary), .noBoundary)
        XCTAssertEqual(NMPHistoryLoadError(.StoreUnavailable), .storeUnavailable)
        XCTAssertEqual(
            NMPHistoryLoadError(.TransportUnavailable(reason: "offline")),
            .transportUnavailable(reason: "offline")
        )
    }

    private func ffiRow(id: String, createdAt: UInt64) -> FfiRow {
        FfiRow(
            id: id,
            pubkey: "pk",
            createdAt: createdAt,
            kind: 7_779,
            tags: [],
            content: id,
            sig: "sig",
            sources: ["wss://history.example"]
        )
    }

    private static func firstBatch(
        from query: NMPHistoryQuery,
        timeoutSeconds: UInt64
    ) async -> HistoryBatch? {
        await withTaskGroup(of: HistoryBatch?.self) { group in
            group.addTask {
                var iterator = query.makeAsyncIterator()
                return await iterator.next()
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

    private static func value<T>(
        of task: Task<T, Never>,
        timeoutSeconds: UInt64
    ) async -> T? {
        await withTaskGroup(of: T?.self) { group in
            group.addTask { await task.value }
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
