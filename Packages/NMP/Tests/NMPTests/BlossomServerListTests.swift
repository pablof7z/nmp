import XCTest
@testable import NMP

final class BlossomServerListTests: XCTestCase {
    private func row() -> Row {
        Row(
            id: String(repeating: "11", count: 32),
            pubkey: String(repeating: "22", count: 32),
            createdAt: 10,
            kind: 10063,
            tags: [
                ["server", "http://127.0.0.1:3000"],
                ["server"],
                ["server", "ftp://invalid.example"],
                ["server", "https://8.8.8.8"],
            ],
            content: "not-used",
            sig: String(repeating: "33", count: 64),
            sources: ["wss://relay.example"]
        )
    }

    func testDemandAndDecodeKeepOrdinaryQueryAndMalformedEvidence() {
        let demand = blossomServerListDemand()
        XCTAssertEqual(demand.selection.kinds, [10063])
        XCTAssertEqual(demand.source, .authorOutboxes)
        XCTAssertEqual(demand.access, .public)

        let list = decodeBlossomServerList(row())
        XCTAssertEqual(
            list.servers,
            ["http://127.0.0.1:3000/", "https://8.8.8.8/"]
        )
        XCTAssertEqual(list.serverTagCount, 4)
        XCTAssertEqual(list.malformedEntries.map(\.tagIndex), [1, 2])
        guard case .missingUrl = list.malformedEntries[0].error else {
            return XCTFail("missing URL must remain its own evidence")
        }
        guard case .invalidUrl(.unsupportedScheme(let scheme)) =
            list.malformedEntries[1].error
        else {
            return XCTFail("invalid scheme must remain typed")
        }
        XCTAssertEqual(scheme, "ftp")
        XCTAssertTrue(list.hasUnexpectedContent)
        XCTAssertFalse(list.isSpecCompliant)
    }

    func testSignedLocalHostNeverMintsAdmissionAndProvenanceOrderSurvives() async throws {
        let list = decodeBlossomServerList(row())
        let evidence = try await BlossomClient().qualifyServerCandidates(
            policy: .signedListThenOperator,
            operatorServerURLs: ["https://1.1.1.1"],
            signedList: list
        )
        XCTAssertEqual(evidence.count, 3)
        XCTAssertEqual(evidence.map(\.source), [.signedList, .signedList, .operatorConfig])
        guard case .localHostNotAdmitted(let host) = evidence[0].admission else {
            return XCTFail("signed loopback endpoint must remain refused")
        }
        XCTAssertEqual(host, "127.0.0.1")
        guard case .admitted(_, let signedOverride) = evidence[1].admission else {
            return XCTFail("public signed endpoint should qualify")
        }
        XCTAssertFalse(signedOverride)
        guard case .admitted(_, let operatorOverride) = evidence[2].admission else {
            return XCTFail("public operator endpoint should qualify")
        }
        XCTAssertFalse(operatorOverride)
    }
}
