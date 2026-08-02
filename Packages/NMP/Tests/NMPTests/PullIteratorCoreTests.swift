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

    /// #1192: the second half of #762's guarantee. Acknowledgement (`commit`)
    /// happens before Swift maps the frame, but there is still Swift-side work
    /// -- folding into the accumulator, the #17 cadence delay -- before the app
    /// actually receives the value. If the app's task is cancelled in that
    /// window, the acknowledged transition must not just vanish: the whole
    /// observation must be withdrawn, so no later pull ever continues from a
    /// step the app never got.
    ///
    /// This does not race the window with a sleep or a scheduling assumption.
    /// `AcknowledgeThenCancelPull.commit()` cancels the driving `Task` itself,
    /// synchronously, from inside the exact call that acknowledges the row --
    /// deterministic ordering, not a scheduling claim. A `RowCancelGate` (not a
    /// clock) makes the test wait for the task handle to exist before the pull
    /// is allowed to proceed, so there is no window in which `commit()` could
    /// run before the task reference is stored.
    func testCancellingAfterAcknowledgementWithdrawsTheWholeObservationBeforeDelivery() async throws {
        let settlement = RowTicketSettlement()
        let gate = RowCancelGate()
        let box = RowCancelBox()
        let stream = AcknowledgeThenCancelStream(settlement: settlement, gate: gate, box: box)
        let core = NMPPullIteratorCore(
            handle: stream,
            iteratorGate: NMPPullIteratorGate(),
            throttle: true
        ) { _ in
            settlement.record("map")
            return "mapped"
        }

        let task = Task<String?, Error> { try await core.next() }
        box.store(task)
        gate.open()

        let result = try await task.value
        XCTAssertNil(
            result,
            "a transition acknowledged then cancelled before delivery is never handed to the app"
        )
        XCTAssertEqual(
            settlement.events,
            ["begin", "receive", "commit", "map"],
            "the row was acknowledged, and even folded, before cancellation could be observed"
        )
        XCTAssertEqual(
            stream.cancelCount,
            1,
            "cancellation after acknowledgement withdrew the whole observation exactly once"
        )

        // The property, stated positively rather than by racing the window: a
        // withdrawn observation refuses to continue, so no later pull can ever
        // resume from the transition the app never received.
        let after = try await core.next()
        XCTAssertNil(after, "a withdrawn observation never continues from an unapplied transition")
        XCTAssertEqual(
            stream.cancelCount,
            1,
            "an already-terminal iterator does not withdraw native demand a second time"
        )
    }
}

/// Lets the test establish "the task handle exists" before a pull is allowed
/// to proceed, without a clock: `receive()` suspends here until `open()` is
/// called, so ordering is enforced by an explicit signal, not scheduling luck.
private final class RowCancelGate: @unchecked Sendable {
    private let lock = NSLock()
    private var opened = false
    private var continuation: CheckedContinuation<Void, Never>?

    func wait() async {
        await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
            lock.lock()
            if opened {
                lock.unlock()
                continuation.resume()
            } else {
                self.continuation = continuation
                lock.unlock()
            }
        }
    }

    func open() {
        lock.lock()
        opened = true
        let pending = continuation
        continuation = nil
        lock.unlock()
        pending?.resume()
    }
}

/// Holds the `Task` driving `core.next()` so `commit()` can cancel it from the
/// inside -- the acknowledgement itself is what triggers the cancellation, so
/// there is no gap in which the two could be observed out of order.
private final class RowCancelBox: @unchecked Sendable {
    private let lock = NSLock()
    private var task: Task<String?, Error>?

    func store(_ task: Task<String?, Error>) {
        lock.lock()
        self.task = task
        lock.unlock()
    }

    func cancelStored() {
        lock.lock()
        let stored = task
        lock.unlock()
        stored?.cancel()
    }
}

private final class AcknowledgeThenCancelPull: NmpRowPull, @unchecked Sendable {
    private let settlement: RowTicketSettlement
    private let gate: RowCancelGate
    private let box: RowCancelBox

    init(settlement: RowTicketSettlement, gate: RowCancelGate, box: RowCancelBox) {
        self.settlement = settlement
        self.gate = gate
        self.box = box
        super.init(noPointer: .init())
    }

    required init(unsafeFromRawPointer pointer: UnsafeMutableRawPointer) {
        settlement = RowTicketSettlement()
        gate = RowCancelGate()
        box = RowCancelBox()
        super.init(unsafeFromRawPointer: pointer)
    }

    override func receive() async throws -> FfiFrame? {
        await gate.wait()
        settlement.record("receive")
        return FfiFrame(
            deltas: [],
            window: nil,
            evidence: [FfiAcquisitionEvidence(sources: [], shortfall: [])]
        )
    }

    override func commit() throws {
        settlement.record("commit")
        // The acknowledgement itself requests cancellation of the app's task.
        // Because `pull.receive()` has already returned, this is the last
        // cancellable suspension point still open on this call stack, so
        // `withTaskCancellationHandler`'s `onCancel` fires synchronously here.
        box.cancelStored()
    }

    override func abort() {
        settlement.record("abort")
    }
}

private final class AcknowledgeThenCancelStream: NmpRowStream, @unchecked Sendable {
    private let settlement: RowTicketSettlement
    private let gate: RowCancelGate
    private let box: RowCancelBox
    private let lock = NSLock()
    private var recordedCancelCalls = 0

    init(settlement: RowTicketSettlement, gate: RowCancelGate, box: RowCancelBox) {
        self.settlement = settlement
        self.gate = gate
        self.box = box
        super.init(noPointer: .init())
    }

    required init(unsafeFromRawPointer pointer: UnsafeMutableRawPointer) {
        settlement = RowTicketSettlement()
        gate = RowCancelGate()
        box = RowCancelBox()
        super.init(unsafeFromRawPointer: pointer)
    }

    var cancelCount: Int {
        lock.lock()
        defer { lock.unlock() }
        return recordedCancelCalls
    }

    override func beginNext() throws -> NmpRowPull {
        settlement.record("begin")
        return AcknowledgeThenCancelPull(settlement: settlement, gate: gate, box: box)
    }

    override func cancel() {
        lock.lock()
        recordedCancelCalls += 1
        lock.unlock()
    }

    override func requestRows(atLeast: UInt64) throws {}
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
