// #485: windowing is a policy on `observe`, not a parallel noun. These
// tests prove the Swift half of that collapse: the ONE `RowBridge` handles
// both frame shapes (unbounded delta folding, windowed authoritative
// replacement), `requestRows` is the declarative growth verb with typed
// synchronous refusals only, validation failures stay typed on `NMPError`,
// and windowed teardown keeps the same native-task and shutdown discipline
// the unbounded read noun already guarantees.

import NMPFFI
import XCTest
@testable import NMP

final class WindowTests: XCTestCase {
    private static let emptyEvidence = FfiAcquisitionEvidence(sources: [], shortfall: [])

    // MARK: - Bridge: windowed frames replace, unbounded frames fold

    /// A windowed frame is an authoritative snapshot: the bridge must
    /// REPLACE its row state wholesale from `window.rows` and surface the
    /// frame's load fact. Windowed frames carry empty `deltas` by contract
    /// (rows never cross the FFI twice) -- so if the bridge folded deltas
    /// instead of replacing, the third frame here (which drops "b" and adds
    /// "c" with NO delta saying so) would deliver stale rows.
    func testWindowedFramesReplaceRowStateAndCarryLoadFacts() async throws {
        var continuation: AsyncStream<RowBatch>.Continuation!
        let stream = AsyncStream<RowBatch> { continuation = $0 }
        let bridge = RowBridge(continuation: continuation)
        let a = ffiRow(id: "a", createdAt: 300)
        let b = ffiRow(id: "b", createdAt: 200)
        let c = ffiRow(id: "c", createdAt: 400)

        bridge.onFrame(
            frame: FfiFrame(
                deltas: [],
                window: FfiWindowContents(rows: [a], load: .idle),
                evidence: Self.emptyEvidence
            )
        )
        bridge.onFrame(
            frame: FfiFrame(
                deltas: [],
                window: FfiWindowContents(rows: [a, b], load: .returned(added: 1)),
                evidence: Self.emptyEvidence
            )
        )
        bridge.onFrame(
            frame: FfiFrame(
                deltas: [],
                window: FfiWindowContents(rows: [c, a], load: .requesting),
                evidence: Self.emptyEvidence
            )
        )
        bridge.onClosed()

        var delivered: [RowBatch] = []
        for await batch in stream {
            delivered.append(batch)
        }
        XCTAssertFalse(delivered.isEmpty)
        XCTAssertTrue(delivered.allSatisfy { $0.rows.count <= 2 })
        XCTAssertTrue(delivered.allSatisfy { $0.load != nil })
        XCTAssertEqual(delivered.last?.rows.map(\.id), ["c", "a"])
        XCTAssertEqual(delivered.last?.load, .requesting)
    }

    /// The unbounded shape through the SAME observer: `window == nil`
    /// frames keep today's exact delta folding (add, grow-in-place,
    /// remove), and their batches carry no window fact (`load == nil`).
    func testUnboundedFramesStillFoldDeltas() async throws {
        var continuation: AsyncStream<RowBatch>.Continuation!
        let stream = AsyncStream<RowBatch> { continuation = $0 }
        let bridge = RowBridge(continuation: continuation)
        let a = ffiRow(id: "a", createdAt: 300)
        let b = ffiRow(id: "b", createdAt: 200)

        bridge.onFrame(
            frame: FfiFrame(
                deltas: [.added(row: a), .added(row: b)],
                window: nil,
                evidence: Self.emptyEvidence
            )
        )
        bridge.onFrame(
            frame: FfiFrame(
                deltas: [
                    .removed(id: "a"),
                    .sourcesGrew(id: "b", sources: ["wss://r0.example", "wss://r1.example"]),
                ],
                window: nil,
                evidence: Self.emptyEvidence
            )
        )
        bridge.onClosed()

        var delivered: [RowBatch] = []
        for await batch in stream {
            delivered.append(batch)
        }
        XCTAssertFalse(delivered.isEmpty)
        XCTAssertTrue(delivered.allSatisfy { $0.load == nil })
        XCTAssertEqual(delivered.last?.rows.map(\.id), ["b"])
        XCTAssertEqual(
            delivered.last?.rows.first?.sources,
            ["wss://r0.example", "wss://r1.example"]
        )
    }

    // MARK: - Validation, growth refusals, and teardown discipline

    func testWindowValidationAndCancellationReturnNativeTaskBaseline() async throws {
        let engine = try NMPEngine(config: NMPConfig(maxNativeTasks: 1))
        defer { engine.shutdown() }
        let demand = NMPDemand(
            selection: NMPFilter(kinds: [7_779]),
            source: .public
        )

        XCTAssertThrowsError(
            try engine.observe(demand, window: .expandable(initial: 0, max: 2))
        ) { error in
            XCTAssertEqual(error as? NMPError, .windowZeroRows)
        }
        XCTAssertThrowsError(
            try engine.observe(demand, window: .expandable(initial: 3, max: 2))
        ) { error in
            XCTAssertEqual(
                error as? NMPError,
                .windowInitialExceedsMax(initial: 3, max: 2)
            )
        }
        let limited = NMPDemand(
            selection: NMPFilter(kinds: [7_779], limit: 1),
            source: .public
        )
        XCTAssertThrowsError(
            try engine.observe(limited, window: .expandable(initial: 1, max: 2))
        ) { error in
            XCTAssertEqual(error as? NMPError, .windowSelectionHasLimit)
        }

        let query = try engine.observe(demand, window: .expandable(initial: 1, max: 2))
        XCTAssertEqual(engine.nativeTaskCensus().admitted, 1)
        let first = await Self.firstBatch(from: query, timeoutSeconds: 5)
        XCTAssertEqual(first?.rows, [])
        XCTAssertEqual(first?.load, .idle)

        query.cancel()
        engine.awaitNativeTasksIdle()
        XCTAssertEqual(engine.nativeTaskCensus().admitted, 0)
        XCTAssertEqual(engine.nativeTaskCensus().running, 0)
    }

    /// The growth capability's existence is DERIVED from the window policy:
    /// a query opened without a window has nothing to grow and refuses
    /// synchronously with the typed `.unwindowed`.
    func testRequestRowsOnAnUnwindowedQueryThrowsUnwindowed() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }
        let query = try engine.observe(NMPFilter(kinds: [7_781]))

        XCTAssertThrowsError(try query.requestRows(atLeast: 10)) { error in
            XCTAssertEqual(error as? NMPRequestRowsError, .unwindowed)
        }
        query.cancel()
    }

    /// `requestRows` is monotonic, idempotent, and clamped: asking past the
    /// declared `max` never throws -- the bound arrives IN-BAND as the
    /// `.atBound(max:)` fact on a delivered batch (the caller always gets a
    /// beat), and repeating the request stays a no-op `Ok`.
    func testRequestRowsPastTheBoundDeliversAtBoundAsAFactNotAnError() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }
        let demand = NMPDemand(
            selection: NMPFilter(kinds: [7_782]),
            source: .public
        )
        let query = try engine.observe(demand, window: .expandable(initial: 1, max: 1))
        _ = await Self.firstBatch(from: query, timeoutSeconds: 5)

        XCTAssertNoThrow(try query.requestRows(atLeast: 2))
        // Idempotent: the same declarative target again is a plain no-op.
        XCTAssertNoThrow(try query.requestRows(atLeast: 2))

        let atBound = await Self.firstBatch(
            from: query,
            timeoutSeconds: 5,
            where: { $0.load == .atBound(max: 1) }
        )
        XCTAssertEqual(atBound?.load, .atBound(max: 1))
        query.cancel()
    }

    func testEngineShutdownClosesAWindowedIteratorWithinBound() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        let demand = NMPDemand(
            selection: NMPFilter(kinds: [7_780]),
            source: .public
        )
        let query = try engine.observe(demand, window: .expandable(initial: 1, max: 2))
        _ = await Self.firstBatch(from: query, timeoutSeconds: 5)

        let closed = Task {
            var iterator = query.makeAsyncIterator()
            while await iterator.next() != nil {}
            return true
        }
        engine.shutdown()
        let didClose = await Self.value(of: closed, timeoutSeconds: 5) ?? false
        XCTAssertTrue(didClose)
    }

    // MARK: - Typed error parity

    func testEveryRequestRowsFailureKeepsItsTypedAxis() {
        XCTAssertEqual(NMPRequestRowsError(.Unwindowed), .unwindowed)
        XCTAssertEqual(NMPRequestRowsError(.EngineClosed), .engineClosed)
        XCTAssertEqual(NMPRequestRowsError(.StoreUnavailable), .storeUnavailable)
        XCTAssertEqual(
            NMPRequestRowsError(.TransportUnavailable(reason: "offline")),
            .transportUnavailable(reason: "offline")
        )
    }

    func testEveryWindowLoadFactMapsWithoutCollapsing() {
        XCTAssertEqual(WindowLoad(.idle), .idle)
        XCTAssertEqual(WindowLoad(.requesting), .requesting)
        XCTAssertEqual(WindowLoad(.returned(added: 3)), .returned(added: 3))
        XCTAssertEqual(WindowLoad(.atBound(max: 7)), .atBound(max: 7))
        // `.returned(added: 0)` is a distinct honest fact ("the planned
        // advance added nothing"), never conflated with the bound.
        XCTAssertNotEqual(WindowLoad(.returned(added: 0)), WindowLoad(.atBound(max: 0)))
    }

    func testEveryWindowValidationFailureKeepsItsTypedAxis() {
        XCTAssertEqual(NMPError(.WindowZeroRows), .windowZeroRows)
        XCTAssertEqual(
            NMPError(.WindowInitialExceedsMax(initial: 3, max: 2)),
            .windowInitialExceedsMax(initial: 3, max: 2)
        )
        XCTAssertEqual(NMPError(.WindowSelectionHasLimit), .windowSelectionHasLimit)
    }

    // MARK: - Fixtures

    private func ffiRow(id: String, createdAt: UInt64) -> FfiRow {
        FfiRow(
            id: id,
            pubkey: "pk",
            createdAt: createdAt,
            kind: 7_779,
            tags: [],
            content: id,
            sig: "sig",
            sources: ["wss://window.example"]
        )
    }

    private static func firstBatch(
        from query: NMPQuery,
        timeoutSeconds: UInt64,
        where matches: @escaping @Sendable (RowBatch) -> Bool = { _ in true }
    ) async -> RowBatch? {
        await withTaskGroup(of: RowBatch?.self) { group in
            group.addTask {
                var iterator = query.makeAsyncIterator()
                while let batch = await iterator.next() {
                    if matches(batch) {
                        return batch
                    }
                }
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
