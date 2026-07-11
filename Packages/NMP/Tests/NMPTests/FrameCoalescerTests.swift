// Bounded, no-network unit tests for `FrameCoalescer` (#17 Swift-delivery
// half; docs/known-gaps.md's "Unbounded historical replay can peg the main
// thread" entry). These exercise the coalescer directly rather than through
// a live `NMPQuery`/`NMPDiagnostics` (which would need a live relay to
// produce a genuine delta burst) -- `RowBridge`/`DiagnosticsBridge` are thin
// wrappers around this type, so proving the coalescer's contract proves the
// bridges' delivery-cadence fix.

import XCTest
@testable import NMP

final class FrameCoalescerTests: XCTestCase {
    /// A burst of pushes arriving far faster than the coalescing interval
    /// (the shape of historical replay flooding `onBatch`/`onSnapshot`) must
    /// collapse into far fewer deliveries than pushes, while the LAST
    /// delivered value is the LAST pushed value -- no delta is ever lost
    /// from the final state, only intermediate *deliveries* are dropped.
    func testBurstCoalescesIntoFewDeliveries() async throws {
        let box = DeliveredBox()
        let coalescer = FrameCoalescer<Int>(interval: .milliseconds(20)) { value in
            Task { await box.append(value) }
        }

        let pushCount = 200
        for i in 0..<pushCount {
            coalescer.push(i)
        }

        // Give every scheduled flush plenty of time to fire.
        try await Task.sleep(for: .milliseconds(500))

        let delivered = await box.values
        XCTAssertFalse(delivered.isEmpty, "must deliver at least one coalesced value")
        XCTAssertLessThan(
            delivered.count, pushCount / 4,
            "a tight-loop burst must collapse into far fewer deliveries than pushes -- "
                + "got \(delivered.count) deliveries for \(pushCount) pushes"
        )
        XCTAssertEqual(
            delivered.last, pushCount - 1,
            "the final delivered value must be the LAST pushed value -- correctness of the "
                + "accumulated/final state is preserved even though intermediate deliveries "
                + "are dropped"
        )
    }

    /// Pushes spaced further apart than the coalescing interval are each
    /// delivered individually -- coalescing only kicks in for a genuine
    /// burst, it never adds latency to an already-unhurried producer.
    func testSpacedPushesAreEachDelivered() async throws {
        let box = DeliveredBox()
        let coalescer = FrameCoalescer<Int>(interval: .milliseconds(15)) { value in
            Task { await box.append(value) }
        }

        for i in 0..<3 {
            coalescer.push(i)
            try await Task.sleep(for: .milliseconds(60))
        }
        try await Task.sleep(for: .milliseconds(100))

        let delivered = await box.values
        XCTAssertEqual(delivered, [0, 1, 2])
    }

    /// `flushNow` delivers a still-pending value immediately instead of
    /// waiting out the scheduled window -- the exact mechanism `onClosed`
    /// relies on so a burst landing right at stream close is never dropped.
    func testFlushNowDeliversPendingValueImmediately() async throws {
        let box = DeliveredBox()
        let coalescer = FrameCoalescer<Int>(interval: .seconds(10)) { value in
            Task { await box.append(value) }
        }

        // Prime: the FIRST push on a fresh coalescer delivers immediately
        // (no prior delivery to measure elapsed time against), establishing
        // a baseline so the next pushes land inside a real 10s window.
        coalescer.push(0)
        try await Task.sleep(for: .milliseconds(50))
        var delivered = await box.values
        XCTAssertEqual(delivered, [0])

        // Both land well inside the 10s window -- the scheduled flush is
        // still sleeping, not yet due.
        coalescer.push(1)
        coalescer.push(2)
        try await Task.sleep(for: .milliseconds(50))
        delivered = await box.values
        XCTAssertEqual(delivered, [0], "must not have delivered yet -- still inside the window")

        coalescer.flushNow()
        try await Task.sleep(for: .milliseconds(50))
        delivered = await box.values
        XCTAssertEqual(
            delivered, [0, 2],
            "flushNow must deliver the latest pending value (2), coalescing away 1"
        )
    }
}

/// Thread-safe collector for values delivered from `FrameCoalescer`'s
/// background delivery `Task`s.
private actor DeliveredBox {
    private(set) var values: [Int] = []
    func append(_ value: Int) {
        values.append(value)
    }
}
