import XCTest
@testable import NMP

final class ReceiptSetTests: XCTestCase {
    func testExactCapacityAndTaggedAbsenceCrossTheSwiftSurface() async throws {
        let engine = try NMPEngine(config: .init())
        defer { engine.shutdown() }

        XCTAssertEqual(engine.receiptSetCapacity, 32)
        let exact = (1...engine.receiptSetCapacity).map(ReceiptSetIdentity.id)
        let sequence = try engine.observeReceipts(exact)
        var iterator = sequence.makeAsyncIterator()
        guard case .notFound(.id(let receiptId)) = try await iterator.next() else {
            return XCTFail("expected one tagged not-found outcome")
        }
        XCTAssertEqual(receiptId, 1)

        let plusOne = (1...(engine.receiptSetCapacity + 1)).map(ReceiptSetIdentity.id)
        XCTAssertThrowsError(try engine.observeReceipts(plusOne)) { error in
            XCTAssertEqual(
                error as? NMPReceiptSetError,
                .capacityExceeded(capacity: 32, requested: 33)
            )
        }
    }
}
