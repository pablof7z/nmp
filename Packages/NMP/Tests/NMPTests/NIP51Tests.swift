import XCTest

@testable import NMP

final class NIP51Tests: XCTestCase {
    private func fabricatedRow(kind: UInt16) -> Row {
        Row(
            id: "caller-chosen-id", pubkey: "caller-chosen-pubkey",
            createdAt: 1, kind: kind,
            tags: [
                ["group", "group-a", "wss://relay-a.example.com", "Group A"],
                ["group", "missing-relay"],
                ["r", "wss://relay-in-use.example.com"],
            ],
            content: "encrypted-private-items", sig: "caller-chosen-signature", sources: []
        )
    }

    /// #863: a row the app fabricated -- wrong kind, invented signature, no
    /// relay sources -- still parses, still reports its evidence, and still
    /// yields nothing but data.
    func testTolerantParserPreservesEvidenceForFabricatedWrongKindRow() {
        let list = NMP.parseSimpleGroupsListTolerant(fabricatedRow(kind: 1))
        XCTAssertEqual(list.items.count, 1)
        XCTAssertEqual(list.items[0].groupId, "group-a")
        XCTAssertEqual(list.items[0].hostRelay, "wss://relay-a.example.com")
        XCTAssertEqual(list.items[0].name, "Group A")
        XCTAssertEqual(list.relaysInUse, ["wss://relay-in-use.example.com"])
        XCTAssertEqual(list.malformedItemCount, 1)
        XCTAssertTrue(list.hasPrivateContent)

        // The kind:10009 spelling buys the value nothing extra.
        XCTAssertEqual(NMP.parseSimpleGroupsListTolerant(fabricatedRow(kind: 10009)), list)
    }

    /// Browsing a group takes a host the app explicitly supplies; the parsed
    /// value never becomes routing authority on its own.
    func testGroupBrowsingStillTakesAnExplicitlySuppliedHost() throws {
        let list = NMP.parseSimpleGroupsListTolerant(fabricatedRow(kind: 10009))
        let demand = try NMP.groupDiscoveryDemand(host: list.items[0].hostRelay)
        XCTAssertEqual(demand.selection.kinds, [39000])
    }
}
