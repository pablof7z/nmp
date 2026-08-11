// The native NIP-29 relay-scope/group/predicate projection (#1033).

import XCTest
@testable import NMP

final class NIP29Tests: XCTestCase {
    private func host(_ n: Int) -> String {
        "wss://host-\(n).example.com"
    }

    func testOnRejectsAnEmptyRelaySet() {
        XCTAssertThrowsError(try NMPRelayScope.on([])) { error in
            guard case NMPError.emptyRelayScope = error else {
                return XCTFail("expected .emptyRelayScope, got \(error)")
            }
        }
    }

    func testOnRejectsAnUnparseableHost() {
        XCTAssertThrowsError(try NMPRelayScope.on(["not-a-url"])) { error in
            guard case NMPError.invalidRelayUrl(let got) = error else {
                return XCTFail("expected .invalidRelayUrl, got \(error)")
            }
            XCTAssertEqual(got, "not-a-url")
        }
    }

    /// A multi-host group read is ONE live query with one complete branch
    /// per host, each pinned to that host alone and scoped by `#h`.
    func testGroupReadIsOneBranchPerHostPinnedToThatHost() throws {
        let scope = try NMPRelayScope.on([host(1), host(2)])
        let group = scope.group("photographers")

        let query = try group.read(NMPFilter())
        XCTAssertEqual(query.branches.count, 2)
        for (branch, expectedHost) in zip(query.branches, [host(1), host(2)]) {
            guard case .pinned(let relays) = branch.source else {
                return XCTFail("expected .pinned, got \(branch.source)")
            }
            XCTAssertEqual(relays, [expectedHost])
            XCTAssertEqual(branch.access, .public)
            guard case .literal(let values) = branch.selection.tags["h"] else {
                return XCTFail("expected an h tag literal binding")
            }
            XCTAssertEqual(values, ["photographers"])
        }
        XCTAssertNil(query.aggregateResultLimit)
    }

    /// A read selection that already constrains `#h` is refused before any
    /// live query is formed -- the retained group id is the sole semantic
    /// source of that row.
    func testGroupReadNamingItsOwnHRowIsRefused() throws {
        let scope = try NMPRelayScope.on([host(1)])
        let group = scope.group("photographers")

        var selection = NMPFilter()
        selection.tags = ["h": .literal(["elsewhere"])]

        XCTAssertThrowsError(try group.read(selection)) { error in
            guard case NMPError.groupCallerSuppliedContextConstraint = error else {
                return XCTFail("expected .groupCallerSuppliedContextConstraint, got \(error)")
            }
        }
    }

    /// The composable predicate door: union/intersect/minus fold through
    /// the grammar's own set algebra, and a multi-host listing still yields
    /// one branch per host.
    func testPredicatesComposeIncludingTheLiteralIdLeaf() throws {
        let member = try NMPGroupIds.memberListIncludes(.reactive(.activePubkey))
        let admin = try NMPGroupIds.adminListIncludes(.reactive(.activePubkey))
        // Composition is total: every combinator returns an id source the
        // records door takes, including the literal-id leaf an app uses for
        // rooms it already knows about.
        _ = try member.union([admin, NMPGroupIds.anyOf(.literal(["photographers"]))])
        _ = member.intersect([admin])
        _ = member.minus([admin])
        _ = NMPGroupPredicate.naming(member)
    }

    /// The #1252 capability, in the shape 29er's channel sidebar needs it:
    /// "every room this relay advertises", bounded per host, phrased with no
    /// id set of the app's own.
    func testADirectoryNeedsNoIdSetOfItsOwn() throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }
        let scope = try NMPRelayScope.on([host(1), host(2)])
        let watching = try scope.observeRecords(
            engine: engine, matching: .all, records: [.metadata], limit: 250
        )
        watching.cancel()
    }

    /// The general spelling is reachable from Swift, and its refusal
    /// survives the boundary: a group host is authoritative for NIP-29's
    /// three relay-signed records and nothing else.
    func testTheGeneralSpellingRefusesAKindTheHostDoesNotOwn() throws {
        _ = try NMPGroupIds.whoseRecordMatches(NMPFilter(kinds: [39_002]))
        XCTAssertThrowsError(
            try NMPGroupIds.whoseRecordMatches(NMPFilter(kinds: [10_009]))
        ) { error in
            guard case NMPError.groupIdSelectionNotAGroupRecordKind(let kind) = error else {
                return XCTFail("expected .groupIdSelectionNotAGroupRecordKind, got \(error)")
            }
            XCTAssertEqual(kind, 10_009)
        }
    }

    /// A non-hex literal subject is a typed invalid-public-key refusal --
    /// the same rule `NMPFilter.authors` carries.
    func testANonHexLiteralSubjectIsATypedInvalidPublicKey() {
        XCTAssertThrowsError(
            try NMPGroupIds.memberListIncludes(.literal(["not-a-pubkey"]))
        ) { error in
            guard case NMPError.invalidPublicKey(let got) = error else {
                return XCTFail("expected .invalidPublicKey, got \(error)")
            }
            XCTAssertEqual(got, "not-a-pubkey")
        }
    }

    /// Every named group operation reaches the one publish door, headless
    /// (no relay needs to be reachable for the write to be ACCEPTED at the
    /// engine's door).
    func testEveryNamedGroupOperationReachesTheOnePublishDoor() throws {
        let engine = try NMPEngine(config: NMPConfig())
        let scope = try NMPRelayScope.on([host(1), host(2)])
        let group = scope.group("photographers")
        let authorHex = randomPubkeyHex()
        let subjectHex = randomPubkeyHex()

        XCTAssertNoThrow(
            try group.publish(engine: engine, authorPubkeyHex: authorHex, kind: 9, content: "first light")
        )
        XCTAssertNoThrow(
            try group.joinRequest(engine: engine, authorPubkeyHex: authorHex, inviteCode: "code")
        )
        XCTAssertNoThrow(try group.leaveRequest(engine: engine, authorPubkeyHex: authorHex))
        XCTAssertNoThrow(
            try group.addUsers(
                engine: engine,
                authorPubkeyHex: authorHex,
                users: [NMPGroupUser(pubkeyHex: subjectHex)]
            )
        )
        XCTAssertNoThrow(
            try group.removeUsers(
                engine: engine,
                authorPubkeyHex: authorHex,
                pubkeysHex: [subjectHex]
            )
        )
        XCTAssertNoThrow(
            try group.editMetadata(
                engine: engine, authorPubkeyHex: authorHex,
                edit: NMPGroupMetadataEdit(name: "Photographers"))
        )
        XCTAssertNoThrow(
            try group.deleteEvent(engine: engine, authorPubkeyHex: authorHex, eventID: String(repeating: "09", count: 32))
        )
        XCTAssertNoThrow(try group.createGroup(engine: engine, authorPubkeyHex: authorHex))
        // #1301: the same door states a subgroup's parent on the create itself.
        XCTAssertNoThrow(
            try group.createGroup(engine: engine, authorPubkeyHex: authorHex, parent: "darkroom")
        )
        XCTAssertNoThrow(try group.deleteGroup(engine: engine, authorPubkeyHex: authorHex))
        XCTAssertNoThrow(
            try group.createInvite(engine: engine, authorPubkeyHex: authorHex, code: "code")
        )
    }

    /// A group write returns the ORDINARY `Receipt` -- store-issued id and
    /// all (#1244). There is no group-shaped receipt type left.
    func testAGroupWriteCarriesTheStoreIssuedReceiptID() throws {
        let engine = try NMPEngine(config: NMPConfig())
        let scope = try NMPRelayScope.on([host(1)])
        let group = scope.group("photographers")
        let receipt = try group.publish(
            engine: engine, authorPubkeyHex: randomPubkeyHex(), kind: 9, content: "hi"
        )
        XCTAssertGreaterThan(receipt.id, 0)
    }

    /// #1281: a several-group write reaches the one publish door and comes
    /// back with the ordinary store-issued receipt id.
    func testASeveralGroupWriteReachesTheOnePublishDoor() throws {
        let engine = try NMPEngine(config: NMPConfig())
        let rooms = try NMPRelayScope.on([host(1), host(2)]).groups(["darkroom", "photographers"])
        let receipt = try rooms.publish(
            engine: engine, authorPubkeyHex: randomPubkeyHex(), kind: 30315,
            tags: [["d", "status"]])
        XCTAssertGreaterThan(receipt.id, 0)
    }

    /// #1281: naming no group at all forms no write context.
    func testAWriteContextOverNoGroupIsNeverFormed() throws {
        let scope = try NMPRelayScope.on([host(1)])
        XCTAssertThrowsError(try scope.groups([])) { error in
            guard case NMPError.emptyGroupSet = error else {
                return XCTFail("expected .emptyGroupSet, got \(error)")
            }
        }
    }

    /// A caller-supplied `h` tag never reaches the door: the refusal is
    /// synchronous and typed, before any receipt stream exists.
    func testACallerSuppliedContextNeverReachesTheDoor() throws {
        let engine = try NMPEngine(config: NMPConfig())
        let authorHex = randomPubkeyHex()
        let scope = try NMPRelayScope.on([host(1)])
        let group = scope.group("photographers")

        XCTAssertThrowsError(
            try group.publish(
                engine: engine, authorPubkeyHex: authorHex, kind: 9,
                tags: [["h", "photographers"]]
            )
        ) { error in
            guard case NMPError.groupCallerSuppliedContext = error else {
                return XCTFail("expected .groupCallerSuppliedContext, got \(error)")
            }
        }
    }

    /// `deleteEvent`'s `eventID` is parsed with the same typed
    /// `.invalidEventId` rule every other exact-hex event id input uses.
    func testDeleteEventRejectsAMalformedEventID() throws {
        let engine = try NMPEngine(config: NMPConfig())
        let authorHex = randomPubkeyHex()
        let scope = try NMPRelayScope.on([host(1)])
        let group = scope.group("photographers")

        XCTAssertThrowsError(
            try group.deleteEvent(engine: engine, authorPubkeyHex: authorHex, eventID: "not-an-event-id")
        ) { error in
            guard case NMPError.invalidEventId(let got) = error else {
                return XCTFail("expected .invalidEventId, got \(error)")
            }
            XCTAssertEqual(got, "not-an-event-id")
        }
    }

    private func randomPubkeyHex() -> String {
        String((0..<64).map { _ in "0123456789abcdef".randomElement()! })
    }
}
