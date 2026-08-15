import XCTest

@testable import NMP

final class NIP29SimpleGroupsTests: XCTestCase {
    private let secretKey = String(repeating: "0", count: 63) + "1"

    private func fabricatedRow(kind: UInt16) -> Row {
        Row(
            id: "caller-chosen-id", pubkey: "caller-chosen-pubkey",
            createdAt: 1, kind: kind,
            tags: [
                ["group", "group-a", "wss://relay-a.example.com", "Group A"],
                ["group", "missing-relay"],
                ["r", "wss://relay-in-use.example.com"],
            ],
            content: "encrypted-private-items",
            signature: .signed(signature: "caller-chosen-signature"), sources: []
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

    func testCurrentAccountDemandTargetsKind10009() {
        let demand = NMP.currentAccountGroupListDemand()
        XCTAssertEqual(demand.selection.kinds, [10009])
    }

    func testTypedGroupListActionReturnsTheOrdinaryReceipt() throws {
        let engine = try NMPEngine(
            config: NMPConfig(
                outboxRouting: OutboxRoutingConfig(indexers: ["wss://indexer.example"])
            )
        )
        defer { engine.shutdown() }
        _ = try engine.session.add(
            privateKey: testPrivateKey(secretKey),
            makeCurrent: true
        )

        let receipt = try engine.addGroupToList(
            groupId: "room",
            hostRelay: "wss://host.example",
            name: "Room"
        )
        let queue = try engine.publishQueue(limit: .max)
        XCTAssertEqual(queue.count, 1)
        XCTAssertEqual(queue[0].receiptID, receipt.id)
    }

    func testProviderlessGroupListActionRefusesBeforeCustody() throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }
        _ = try engine.session.add(
            privateKey: testPrivateKey(secretKey),
            makeCurrent: true
        )

        XCTAssertThrowsError(try engine.addRelayInUse("wss://relay.example")) { error in
            XCTAssertEqual(error as? GroupListActionError, .automaticRoutingUnavailable)
        }
        XCTAssertTrue(try engine.publishQueue(limit: .max).isEmpty)
    }

    /// Browsing a group takes a host the app explicitly supplies; the parsed
    /// value never becomes routing authority on its own.
    ///
    /// #858's Swift falsifier too, updated for #1033: the selected
    /// `SimpleGroupEntry` feeds NIP-29's host-scoped door
    /// (`NMPRelayScope.on`/`.group`) directly, with no NIP-29 group-reference
    /// copy of the NIP-51 value in between.
    func testGroupBrowsingStillTakesAnExplicitlySuppliedHost() throws {
        let list = NMP.parseSimpleGroupsListTolerant(fabricatedRow(kind: 10009))
        let selected = list.items[0]
        let scope = try NMPRelayScope.on([selected.hostRelay])
        let group = scope.group(selected.groupId)
        let query = try group.read(NMPFilter(kinds: [9]))
        XCTAssertEqual(query.branches.count, 1)
        XCTAssertEqual(query.branches[0].selection.kinds, [9])

        XCTAssertEqual(selected.groupId, "group-a")
    }

    /// #1245: this test used to read kind 39000 through the content door and
    /// assert the request was built faithfully. No 39000 event carries the
    /// group-context row, so that request could never have matched anything --
    /// it is refused now, and the group's own metadata is read through the
    /// records observation instead.
    func testTheGroupsOwnRecordsAreNotReachableThroughTheContentDoor() throws {
        let list = NMP.parseSimpleGroupsListTolerant(fabricatedRow(kind: 10009))
        let selected = list.items[0]
        let group = try NMPRelayScope.on([selected.hostRelay]).group(selected.groupId)
        XCTAssertThrowsError(try group.read(NMPFilter(kinds: [39000]))) { error in
            guard case NMPError.groupRecordsNotContextScoped(let kinds) = error else {
                return XCTFail("expected .groupRecordsNotContextScoped, got \(error)")
            }
            XCTAssertEqual(kinds, [39000])
        }
    }
}
