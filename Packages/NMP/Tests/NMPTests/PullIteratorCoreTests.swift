// Bounded Swift-delivery tests for the iterator-owned #680 bridge. The bridge
// is intentionally demand-driven: no producer Task and no AsyncStream queue
// can run ahead of the app, while snapshot delivery still has the #17 cadence
// bound that prevents tight historical replay from monopolizing a UI loop.

import Foundation
import XCTest
@testable import NMP
import NMPFFI

final class PullIteratorCoreTests: XCTestCase {
    func testOneAppPullPerNativePullWithNoEagerProducerOrSwiftQueue() async throws {
        let handle = IntPullHandle([1, 2, 3])
        let gate = NMPPullIteratorGate()
        var core: NMPPullIteratorCore<IntPullHandle, Int>? =
            NMPPullIteratorCore(handle: handle, iteratorGate: gate) { $0 }

        let first = try await core?.next()
        XCTAssertEqual(first, 1)
        XCTAssertEqual(handle.nextCalls, 1)

        try await Task.sleep(for: .milliseconds(30))
        XCTAssertEqual(
            handle.nextCalls,
            1,
            "nothing pulls or buffers while the app is not awaiting next()"
        )

        core = nil
        XCTAssertEqual(
            handle.cancelCalls,
            1,
            "dropping the iterator core withdraws native demand exactly once"
        )
    }

    func testSnapshotReplayIsCadenceLimitedWithoutBufferingAhead() async throws {
        let handle = IntPullHandle([1, 2])
        let core = NMPPullIteratorCore(
            handle: handle,
            iteratorGate: NMPPullIteratorGate(),
            throttle: true
        ) { $0 }
        let clock = ContinuousClock()

        let first = try await core.next()
        XCTAssertEqual(first, 1)
        let start = clock.now
        let second = try await core.next()
        XCTAssertEqual(second, 2)
        let elapsed = start.duration(to: clock.now)

        XCTAssertGreaterThanOrEqual(
            elapsed,
            .milliseconds(10),
            "rapid complete snapshots are spaced to roughly one UI-frame cadence"
        )
        XCTAssertEqual(handle.nextCalls, 2)
    }

    func testRowTicketCommitsBeforeSwiftMapsTheFrame() async throws {
        let settlement = RowTicketSettlement()
        let handle = TicketedRowStream(settlement: settlement)
        let core = NMPPullIteratorCore(
            handle: handle,
            iteratorGate: NMPPullIteratorGate()
        ) { _ in
            settlement.record("map")
            return "mapped"
        }

        let value = try await core.next()
        XCTAssertEqual(value, "mapped")
        XCTAssertEqual(
            settlement.events,
            ["begin", "receive", "commit", "map"],
            "foreign completion commits synchronously before mapping or another await"
        )
    }
}

private final class IntPullHandle: NMPPullHandle, @unchecked Sendable {
    typealias Frame = Int

    private let lock = NSLock()
    private var frames: [Int]
    private var recordedNextCalls = 0
    private var recordedCancelCalls = 0

    init(_ frames: [Int]) {
        self.frames = frames
    }

    var nextCalls: Int {
        lock.lock()
        defer { lock.unlock() }
        return recordedNextCalls
    }

    var cancelCalls: Int {
        lock.lock()
        defer { lock.unlock() }
        return recordedCancelCalls
    }

    func pullNext() async throws -> Int? {
        takeNext()
    }

    func cancel() {
        lock.lock()
        recordedCancelCalls += 1
        lock.unlock()
    }

    private func takeNext() -> Int? {
        lock.lock()
        defer { lock.unlock() }
        recordedNextCalls += 1
        guard !frames.isEmpty else { return nil }
        return frames.removeFirst()
    }
}

private final class RowTicketSettlement: @unchecked Sendable {
    private let lock = NSLock()
    private var recordedEvents: [String] = []

    var events: [String] {
        lock.lock()
        defer { lock.unlock() }
        return recordedEvents
    }

    func record(_ event: String) {
        lock.lock()
        recordedEvents.append(event)
        lock.unlock()
    }
}

private final class TicketedRowPull: NmpRowPull, @unchecked Sendable {
    private let settlement: RowTicketSettlement

    init(settlement: RowTicketSettlement) {
        self.settlement = settlement
        super.init(noPointer: .init())
    }

    required init(unsafeFromRawPointer pointer: UnsafeMutableRawPointer) {
        settlement = RowTicketSettlement()
        super.init(unsafeFromRawPointer: pointer)
    }

    override func receive() async throws -> FfiFrame? {
        settlement.record("receive")
        return FfiFrame(
            deltas: [],
            window: nil,
            evidence: [FfiAcquisitionEvidence(sources: [], shortfall: [])]
        )
    }

    override func commit() throws {
        settlement.record("commit")
    }

    override func abort() {
        settlement.record("abort")
    }
}

private final class TicketedRowStream: NmpRowStream, @unchecked Sendable {
    private let settlement: RowTicketSettlement

    init(settlement: RowTicketSettlement) {
        self.settlement = settlement
        super.init(noPointer: .init())
    }

    required init(unsafeFromRawPointer pointer: UnsafeMutableRawPointer) {
        settlement = RowTicketSettlement()
        super.init(unsafeFromRawPointer: pointer)
    }

    override func beginNext() throws -> NmpRowPull {
        settlement.record("begin")
        return TicketedRowPull(settlement: settlement)
    }

    override func cancel() {}

    override func requestRows(atLeast: UInt64) throws {}
}
