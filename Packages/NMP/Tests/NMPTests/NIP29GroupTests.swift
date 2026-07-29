import Foundation
import NMPFFI
import XCTest

@testable import NMP

final class NIP29GroupTests: XCTestCase {
    private let secret = String(repeating: "0", count: 63) + "1"

    private func firstStatus(of receipt: Receipt) async throws -> WriteStatus {
        var iterator = receipt.status.makeAsyncIterator()
        return try XCTUnwrap(try await iterator.next())
    }

    func testGroupIsAnOpaqueIdentityThatMintsOrdinaryDemand() throws {
        let group = try NMPGroup(
            host: "wss://groups.example.com",
            id: "photographers"
        )
        let demand = try group.demand(NMPFilter(kinds: [9, 7]))

        XCTAssertEqual(demand.selection.kinds, [7, 9])
        XCTAssertEqual(
            demand.source,
            .pinned(["wss://groups.example.com"])
        )
    }

    func testJoinNeedsNoSubscriptionAndUsesOrdinaryReceipt() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }
        let registration = try await engine.addAccount(secretKey: secret)
        try engine.setActiveAccount(registration.publicKey)
        let group = try NMPGroup(
            host: "wss://groups.example.com",
            id: "photographers"
        )

        let receipt = try await group.joinRequest(
            using: engine,
            inviteCode: "dark-slide-42"
        )
        XCTAssertEqual(try await firstStatus(of: receipt), .accepted)
        receipt.status.cancel()
    }

    func testCorrelationReattachesSameReceiptAfterEngineRestart() async throws {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("nmp-group-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(
            at: directory,
            withIntermediateDirectories: true
        )
        defer { try? FileManager.default.removeItem(at: directory) }
        let storePath = directory.appendingPathComponent("nmp.redb").path
        let token = "swift-group-restart"
        var acceptedID: UInt64 = 0

        do {
            let engine = try NMPEngine(config: NMPConfig(storePath: storePath))
            let registration = try await engine.addAccount(secretKey: secret)
            try engine.setActiveAccount(registration.publicKey)
            let group = try NMPGroup(
                host: "wss://groups.example.com",
                id: "photographers"
            )
            let receipt = try await group.publish(
                NMPEventBuilder(kind: 9, content: "restart proof"),
                using: engine,
                correlation: token
            )
            XCTAssertEqual(try await firstStatus(of: receipt), .accepted)
            acceptedID = receipt.id
            receipt.status.cancel()
            engine.shutdown()
        }

        let restarted = try NMPEngine(config: NMPConfig(storePath: storePath))
        defer { restarted.shutdown() }
        switch try restarted.reattachReceipt(correlation: token) {
        case .attached(let receipt):
            XCTAssertEqual(receipt.id, acceptedID)
            receipt.status.cancel()
        case .notFound:
            XCTFail("accepted correlation must survive restart")
        case .retainedButUnreadable:
            XCTFail("accepted correlation evidence must remain readable")
        }
    }

    func testGroupContextErrorsMapWithoutLeakingGeneratedTypes() {
        XCTAssertEqual(
            NMPError(.GroupMissingContext(expected: "photographers")),
            .groupMissingContext(expected: "photographers")
        )
        XCTAssertEqual(
            NMPError(.GroupMismatchedContext(found: "darkroom", expected: "photographers")),
            .groupMismatchedContext(found: "darkroom", expected: "photographers")
        )
        XCTAssertEqual(
            NMPError(.GroupAmbiguousContext(expected: "photographers")),
            .groupAmbiguousContext(expected: "photographers")
        )
    }
}
