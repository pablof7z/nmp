// Bounded Swift-delivery tests for the iterator-owned #680 bridge. The bridge
// is intentionally demand-driven: no producer Task and no AsyncStream queue
// can run ahead of the app, while snapshot delivery still has the #17 cadence
// bound that prevents tight historical replay from monopolizing a UI loop.

import Foundation
import XCTest
@testable import NMP

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

    func next() async throws -> Int? {
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
