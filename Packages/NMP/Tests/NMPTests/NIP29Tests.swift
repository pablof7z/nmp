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

    // MARK: - groupSendIntent / publishComposed (#115)

    func testGroupSendIntentComposesAKindBlindGroupSend() throws {
        // Construction-only: proves the wrapper builds and is callable with
        // arbitrary/unusual kinds -- the live round-trip (reaches only the
        // pinned host, frozen template, read-back) is the Rust falsifier
        // (`pinned_host_write.rs`), never re-driven against a live relay here.
        _ = try NMP.groupSendIntent(
            host: "wss://group-host.example.com",
            groupId: "group-a",
            authorPubkey: String(repeating: "a", count: 64),
            createdAt: 1,
            kind: 9999,
            content: "hi"
        )
    }

    func testGroupSendIntentRejectsAReservedExtraTag() {
        XCTAssertThrowsError(
            try NMP.groupSendIntent(
                host: "wss://group-host.example.com",
                groupId: "group-a",
                authorPubkey: String(repeating: "a", count: 64),
                createdAt: 1,
                kind: 9,
                content: "hi",
                extraTags: [["h", "sneaky"]]
            )
        ) { error in
            guard case NMPError.reservedGroupTag(let got) = error else {
                return XCTFail("expected .reservedGroupTag, got \(error)")
            }
            XCTAssertEqual(got, "h")
        }
    }

    func testGroupSendIntentComposesFromCouriedRows() throws {
        // `recentRows` couriers delivered rows exactly as a live
        // `groupContentDemand` read would render them -- proves the wrapper
        // plumbs `Row` through to the FFI boundary without the caller ever
        // touching an `FfiRow`.
        let recent = Row(
            FfiRow(
                id: String(repeating: "1", count: 64), pubkey: String(repeating: "a", count: 64),
                createdAt: 100, kind: 9, tags: [["h", "group-a"]], content: "earlier", sig: "sig",
                sources: []
            )
        )
        _ = try NMP.groupSendIntent(
            host: "wss://group-host.example.com",
            groupId: "group-a",
            authorPubkey: String(repeating: "a", count: 64),
            createdAt: 200,
            kind: 9,
            content: "hi",
            recentRows: [recent]
        )
    }

    /// Take-once (falsifier 10), no live relay needed: mirrors the Rust FFI
    /// falsifier (`ffi_publish_composed_takes_the_intent_exactly_once`) --
    /// no signer is ever attached, so the first `publishComposed` settles
    /// into the retained `.accepted`/`.awaitingCapability` steady state; a
    /// second call on the SAME `GroupSendIntent` must throw
    /// `.intentAlreadyConsumed`.
    func testPublishComposedTakesTheIntentExactlyOnce() async throws {
        let engine = try NMPEngine(config: NMPConfig())
        defer { engine.shutdown() }

        let pubkey = String(repeating: "a", count: 64)
        try engine.setActiveAccount(pubkey)

        let intent = try NMP.groupSendIntent(
            host: "wss://group-host.example.com",
            groupId: "group-a",
            authorPubkey: pubkey,
            createdAt: 1,
            kind: 9,
            content: "hi"
        )

        let receipt = try await engine.publishComposed(intent)
        let first = await Self.firstStatus(from: receipt, timeoutSeconds: 5)
        XCTAssertEqual(first, .accepted)

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
}
