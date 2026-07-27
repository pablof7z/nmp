// The native NIP-29 projection owns group discovery only (#838).

import XCTest
@testable import NMP

final class NIP29Tests: XCTestCase {
    func testGroupDiscoveryDemandPinsTheParsedHost() throws {
        let demand = try NMP.groupDiscoveryDemand(host: "wss://host-1.example.com")
        XCTAssertEqual(demand.selection.kinds, [39000])
        guard case .pinned(let relays) = demand.source else {
            return XCTFail("expected .pinned, got \(demand.source)")
        }
        XCTAssertEqual(relays, ["wss://host-1.example.com"])
    }

    func testGroupDiscoveryDemandRejectsAnUnparseableHost() {
        XCTAssertThrowsError(try NMP.groupDiscoveryDemand(host: "not-a-url")) { error in
            guard case NMPError.invalidRelayUrl(let got) = error else {
                return XCTFail("expected .invalidRelayUrl, got \(error)")
            }
            XCTAssertEqual(got, "not-a-url")
        }
    }
}
