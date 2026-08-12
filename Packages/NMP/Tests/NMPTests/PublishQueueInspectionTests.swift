import XCTest
@testable import NMP

final class PublishQueueInspectionTests: XCTestCase {
    func testBoundedQueueAndExactEventDoorsCrossTheSwiftSurface() throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }

        XCTAssertTrue(try engine.publishQueue(limit: .max).isEmpty)
        XCTAssertTrue(
            try engine.publishQueue(
                forEventID: String(repeating: "0", count: 64),
                limit: .max
            ).isEmpty
        )

        XCTAssertThrowsError(
            try engine.publishQueue(forEventID: "not-an-event-id", limit: .max)
        ) { error in
            guard case .invalidEventID = error as? NMPPublishQueueError else {
                return XCTFail("expected typed invalid-event-id refusal, got \(error)")
            }
        }
    }
}
