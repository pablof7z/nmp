// #485: windowing is a policy on `observe`, not a parallel noun. These
// tests prove the Swift half of that collapse: the ONE `RowAccumulator` fold
// handles both frame shapes (unbounded delta folding, windowed authoritative
// replacement), `requestRows` is the declarative growth verb with typed
// synchronous refusals only, validation failures stay typed on `NMPError`,
// and windowed teardown works over the #680 pull handle.
//
// #680: observations are pull-based async handles now -- there is no
// native-task census, no `maxNativeTasks`, and no admission ceiling to assert.
// The fold is proven directly on `RowAccumulator` (the exact per-frame mapping
// the iterator runs); the coalescer over the pull loop is proven separately by
// `PullIteratorCoreTests`.

import NMPFFI
import XCTest
@testable import NMP

final class WindowTests: XCTestCase {
    private static let emptyEvidence = FfiAcquisitionEvidence(sources: [], shortfall: [])

    // MARK: - Fold: windowed frames replace, unbounded frames fold

    /// A windowed frame is an authoritative snapshot: the fold must REPLACE
    /// its row state wholesale from `window.rows` and surface the frame's load
    /// fact. Windowed frames carry empty `deltas` by contract (rows never
    /// cross the FFI twice) -- so if the fold folded deltas instead of
    /// replacing, the third frame here (which drops "b" and adds "c" with NO
    /// delta saying so) would deliver stale rows.
    func testWindowedFramesReplaceRowStateAndCarryLoadFacts() {
        let accumulator = RowAccumulator()
        let a = ffiRow(id: "a", createdAt: 300)
        let b = ffiRow(id: "b", createdAt: 200)
        let c = ffiRow(id: "c", createdAt: 400)

        let first = accumulator.fold(
            FfiFrame(
                deltas: [],
                window: FfiWindowContents(rows: [a], load: .idle),
                evidence: Self.emptyEvidence
            )
        )
        let second = accumulator.fold(
            FfiFrame(
                deltas: [],
                window: FfiWindowContents(rows: [a, b], load: .returned(added: 1)),
                evidence: Self.emptyEvidence
            )
        )
        let third = accumulator.fold(
            FfiFrame(
                deltas: [],
                window: FfiWindowContents(rows: [c, a], load: .requesting),
                evidence: Self.emptyEvidence
            )
        )

        XCTAssertEqual(first.rows.map(\.id), ["a"])
        XCTAssertEqual(first.load, .idle)
        XCTAssertEqual(second.rows.map(\.id), ["a", "b"])
        XCTAssertEqual(second.load, .returned(added: 1))
        XCTAssertEqual(third.rows.map(\.id), ["c", "a"])
        XCTAssertEqual(third.load, .requesting)
    }

    /// The unbounded shape through the SAME fold: `window == nil` frames keep
    /// today's exact delta folding (add, grow-in-place, remove), and their
    /// batches carry no window fact (`load == nil`).
    func testUnboundedFramesStillFoldDeltas() {
        let accumulator = RowAccumulator()
        let a = ffiRow(id: "a", createdAt: 300)
        let b = ffiRow(id: "b", createdAt: 200)

        let first = accumulator.fold(
            FfiFrame(
                deltas: [.added(row: a), .added(row: b)],
                window: nil,
                evidence: Self.emptyEvidence
            )
        )
        let second = accumulator.fold(
            FfiFrame(
                deltas: [
                    .removed(id: "a"),
                    .sourcesGrew(id: "b", sources: ["wss://r0.example", "wss://r1.example"]),
                ],
                window: nil,
                evidence: Self.emptyEvidence
            )
        )

        XCTAssertNil(first.load)
        XCTAssertNil(second.load)
        XCTAssertEqual(first.rows.map(\.id), ["a", "b"])
        XCTAssertEqual(second.rows.map(\.id), ["b"])
        XCTAssertEqual(
            second.rows.first?.sources,
            ["wss://r0.example", "wss://r1.example"]
        )
    }

    // MARK: - Validation, growth refusals, and teardown discipline

    /// #680: window validation still fails closed and typed; opening and
    /// cancelling a windowed query has no capacity census to reconcile.
    func testWindowValidationAndCancellationAreTypedWithNoCapacityConcept() async throws {
        let engine = try NMPEngine(config: NMPConfig())
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
        let first = await Self.firstBatch(from: query, timeoutSeconds: 5)
        XCTAssertEqual(first?.rows, [])
        XCTAssertEqual(first?.load, .idle)

        // Cancellation is idempotent and leaves nothing to reconcile (#680).
        query.cancel()
        query.cancel()
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

        // #680: the handle is single-consumer/single-pass -- ONE iterator for
        // the whole read (a second `makeAsyncIterator` would open a second
        // pump on the one handle). Pull the initial batch, grow, then pull the
        // in-band `.atBound` fact -- all on the same iterator, inside one Task
        // raced against a hard timeout so a regression fails loudly.
        let atBound = await Self.value(
            of: Task { () -> WindowLoad? in
                var iterator = query.makeAsyncIterator()
                _ = try? await iterator.next()
                try? query.requestRows(atLeast: 2)
                // Idempotent: the same declarative target again is a no-op.
                try? query.requestRows(atLeast: 2)
                while let batch = try? await iterator.next() {
                    if batch.load == .atBound(max: 1) {
                        return batch.load
                    }
                }
                return nil
            },
            timeoutSeconds: 5
        ) ?? nil
        XCTAssertEqual(atBound, .atBound(max: 1))
        query.cancel()
    }

    func testEngineShutdownClosesAWindowedIteratorWithinBound() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        let demand = NMPDemand(
            selection: NMPFilter(kinds: [7_780]),
            source: .public
        )
        let query = try engine.observe(demand, window: .expandable(initial: 1, max: 2))

        // ONE iterator for the whole read (#680 single-pass handle): drain it
        // to completion; engine shutdown drops the producer and closes it.
        let closed = Task {
            var iterator = query.makeAsyncIterator()
            do {
                while try await iterator.next() != nil {}
            } catch {
                // The stream ended with the single-consumer / withdrawal
                // signal; a closed iterator is exactly what this asserts.
            }
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
                do {
                    var iterator = query.makeAsyncIterator()
                    while let batch = try await iterator.next() {
                        if matches(batch) {
                            return batch
                        }
                    }
                } catch {
                    return nil
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
