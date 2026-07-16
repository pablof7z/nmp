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

    // MARK: - groupMessageIntent / publishComposed (#156)

    func testGroupMessageIntentRequiresAnActiveAccount() throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }

        XCTAssertThrowsError(
            try engine.groupMessageIntent(
                host: "wss://group-host.example.com",
                groupID: "group-a",
                content: "hello"
            )
        ) { error in
            guard case NMPError.noActiveAccount = error else {
                return XCTFail("expected .noActiveAccount, got \(error)")
            }
        }
    }

    /// Crosses the real Swift -> UniFFI -> Rust -> canonical-store path and
    /// reads the accepted row back through an ordinary pinned live query.
    /// This proves Swift supplies only semantic values while NMP owns the
    /// exact author/time/kind/content/tag template.
    func testGroupMessageIntentMaterializesTheCanonicalSemanticTemplate() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }

        let registration = try await engine.addAccount(
            secretKey: String(repeating: "0", count: 63) + "1"
        )
        let author = registration.publicKey
        try engine.setActiveAccount(author)

        let host = "wss://group-host.example.com"
        let groupID = "group-a"
        let first = "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d"
        let second = "7e7e9c42a91bfef19fa929e5fda1b72e0ebc1a4c1141673e2794234d86addf4e"
        let parentID = String(repeating: "1", count: 64)
        let query = try engine.observe(NMP.groupContentDemand(host: host, groupId: groupID))
        let rowTask = Task { await Self.firstRow(from: query, timeoutSeconds: 5) }

        let intent = try engine.groupMessageIntent(
            host: host,
            groupID: groupID,
            content: "hello",
            recipients: [first, first, second],
            reply: GroupReplyParent(eventID: parentID, authorPubkey: first)
        )
        let receipt = try await engine.publishComposed(intent)
        let status = await Self.firstStatus(from: receipt, timeoutSeconds: 5)
        XCTAssertEqual(status, .accepted)

        let row = await rowTask.value
        XCTAssertEqual(row?.pubkey, author)
        XCTAssertEqual(row?.kind, 9)
        XCTAssertGreaterThan(row?.createdAt ?? 0, 1_700_000_000)
        XCTAssertEqual(
            row?.content,
            "nostr:npub180cvv07tjdrrgpa0j7j7tmnyl2yr6yr7l8j4s3evf6u64th6gkwsyjh6w6 " +
            "nostr:npub10elfcs4fr0l0r8af98jlmgdh9c8tcxjvz9qkw038js35mp4dma8qzvjptg hello"
        )
        XCTAssertEqual(
            row?.tags,
            [
                ["p", first],
                ["p", second],
                ["e", parentID, "", "reply", first],
                ["h", groupID],
            ]
        )
    }

    func testGroupMessageIntentRejectsMalformedTypedRecipients() throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }
        try engine.setActiveAccount(String(repeating: "a", count: 64))

        XCTAssertThrowsError(
            try engine.groupMessageIntent(
                host: "wss://group-host.example.com",
                groupID: "group-a",
                content: "hello",
                recipients: ["not-a-pubkey"]
            )
        ) { error in
            guard case NMPError.invalidPublicKey(let got) = error else {
                return XCTFail("expected .invalidPublicKey, got \(error)")
            }
            XCTAssertEqual(got, "not-a-pubkey")
        }
    }

    func testPublishComposedTakesTheIntentExactlyOnce() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }
        try engine.setActiveAccount(String(repeating: "a", count: 64))

        let intent = try engine.groupMessageIntent(
            host: "wss://group-host.example.com",
            groupID: "group-a",
            content: "hi"
        )
        let receipt = try await engine.publishComposed(intent)
        let status = await Self.firstStatus(from: receipt, timeoutSeconds: 5)
        XCTAssertEqual(status, .accepted)

        do {
            _ = try await engine.publishComposed(intent)
            XCTFail("expected the second publishComposed call to throw")
        } catch NMPError.intentAlreadyConsumed {
            // expected
        }
    }

    private static func firstStatus(from receipt: Receipt, timeoutSeconds: UInt64) async -> WriteStatus? {
        await withTaskGroup(of: WriteStatus?.self) { group in
            group.addTask {
                for await status in receipt.status {
                    return status
                }
                return nil
            }
            group.addTask {
                try? await Task.sleep(nanoseconds: timeoutSeconds * 1_000_000_000)
                return nil
            }

            let result = await group.next() ?? nil
            group.cancelAll()
            return result
        }
    }

    private static func firstRow(from query: NMPQuery, timeoutSeconds: UInt64) async -> Row? {
        await withTaskGroup(of: Row?.self) { group in
            group.addTask {
                for await batch in query {
                    if let row = batch.rows.first {
                        return row
                    }
                }
                return nil
            }
            group.addTask {
                try? await Task.sleep(nanoseconds: timeoutSeconds * 1_000_000_000)
                return nil
            }

            let result = await group.next() ?? nil
            group.cancelAll()
            return result
        }
    }
}
