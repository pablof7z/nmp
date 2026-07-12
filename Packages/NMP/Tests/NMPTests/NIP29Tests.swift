// The read-only NIP-29 host-browser projection (#108) -- construction/
// mapping tests only. No network: these are pure Demand constructors and
// a pure decode.

import XCTest
@testable import NMP
import NMPFFI

final class NIP29Tests: XCTestCase {
    func testActiveAccountDemandTargetsKind10009() {
        let demand = NMP.activeAccountDemand()
        XCTAssertEqual(demand.selection.kinds, [10009])
    }

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

    func testGroupContentDemandScopesByHTag() throws {
        let demand = try NMP.groupContentDemand(host: "wss://host-1.example.com", groupId: "group-a")
        XCTAssertEqual(demand.selection.kinds, [9, 30315])
    }

    func testDecodeRememberedGroupsComposesAKind10009Row() {
        let row = Row(
            FfiRow(
                id: "id", pubkey: "pubkey", createdAt: 1, kind: 10009,
                tags: [["group", "group-a", "wss://relay-a.example.com", "Group A"]],
                content: "", sig: "sig", sources: []
            )
        )
        let remembered = NMP.decodeRememberedGroups(row)
        XCTAssertEqual(remembered.groups.count, 1)
        XCTAssertEqual(remembered.groups[0].groupId, "group-a")
        XCTAssertEqual(remembered.groups[0].host, "wss://relay-a.example.com")
        XCTAssertEqual(remembered.groups[0].name, "Group A")
        XCTAssertFalse(remembered.hasPrivateContent)
    }
}
