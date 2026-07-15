import Foundation
import XCTest
@testable import NMP

final class InsecureFileAccountStoreTests: XCTestCase {
    private let secretOne = String(repeating: "0", count: 63) + "1"
    private let publicOne = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"

    func testCheckpointRestoresActiveAccountAndClearReturnsToReadOnly() async throws {
        let fixture = try makeFixture()
        defer { try? FileManager.default.removeItem(at: fixture.root) }

        let first = try NMPEngine(
            config: NMPConfig(),
            localAccountStore: fixture.store
        )
        let pubkey = try await first.addAccount(secretKey: secretOne)
        try first.setActiveAccount(pubkey)
        XCTAssertEqual(try first.activeAccount(), publicOne)
        first.shutdown()

        let permissions = try FileManager.default.attributesOfItem(
            atPath: fixture.checkpoint.path
        )[.posixPermissions] as? NSNumber
        XCTAssertEqual((permissions?.intValue ?? 0) & 0o777, 0o600)

        let restored = try NMPEngine(
            config: NMPConfig(),
            localAccountStore: fixture.store
        )
        XCTAssertEqual(try restored.activeAccount(), publicOne)
        try restored.clearPersistedAccount()
        restored.shutdown()
        XCTAssertFalse(FileManager.default.fileExists(atPath: fixture.checkpoint.path))

        let signedOut = try NMPEngine(
            config: NMPConfig(),
            localAccountStore: fixture.store
        )
        XCTAssertNil(try signedOut.activeAccount())
        signedOut.shutdown()
    }

    /// #507/#495: removing the checkpointed account must clear the on-disk
    /// checkpoint too, or the removed account resurrects on the very next
    /// restore -- proving `addAccount`'s side effect has a symmetric undo.
    /// Removing an unrelated pubkey must leave an existing checkpoint alone.
    func testRemoveAccountClearsCheckpointAndLeavesUnrelatedCheckpointIntact() async throws {
        let fixture = try makeFixture()
        defer { try? FileManager.default.removeItem(at: fixture.root) }
        // The x-only pubkey for secret key `2` -- a distinct, valid, but
        // never-`addAccount`-ed account, used only to prove removal of an
        // unrelated pubkey is a no-op that does not touch the checkpoint.
        let otherPubkey = "c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5"

        let engine = try NMPEngine(config: NMPConfig(), localAccountStore: fixture.store)
        let pubkey = try await engine.addAccount(secretKey: secretOne)
        try engine.setActiveAccount(pubkey)
        XCTAssertTrue(FileManager.default.fileExists(atPath: fixture.checkpoint.path))

        XCTAssertEqual(
            try engine.removeAccount(pubkey: otherPubkey),
            false,
            "an unrelated, never-added pubkey has nothing to remove"
        )
        XCTAssertTrue(
            FileManager.default.fileExists(atPath: fixture.checkpoint.path),
            "removing an unrelated pubkey must not touch an existing checkpoint"
        )

        XCTAssertEqual(try engine.removeAccount(pubkey: pubkey), true)
        XCTAssertFalse(
            FileManager.default.fileExists(atPath: fixture.checkpoint.path),
            "removing the checkpointed account must clear its on-disk checkpoint"
        )
        engine.shutdown()

        let restarted = try NMPEngine(config: NMPConfig(), localAccountStore: fixture.store)
        XCTAssertNil(
            try restarted.activeAccount(),
            "a removed-and-cleared account must not resurrect on the next restart"
        )
        restarted.shutdown()
    }

    func testInvalidCheckpointFailsClosedDuringConstruction() throws {
        let fixture = try makeFixture()
        defer { try? FileManager.default.removeItem(at: fixture.root) }
        try Data("not-a-key".utf8).write(to: fixture.checkpoint)

        XCTAssertThrowsError(
            try NMPEngine(config: NMPConfig(), localAccountStore: fixture.store)
        ) { error in
            XCTAssertEqual(error as? NMPError, .invalidSecretKey)
        }
    }

    func testPersistentStoreResetPreservesAccountCheckpoint() async throws {
        let fixture = try makeFixture()
        defer { try? FileManager.default.removeItem(at: fixture.root) }
        let database = fixture.root.appendingPathComponent("nmp.redb")
        let config = NMPConfig(storePath: database.path)

        let first = try NMPEngine(config: config, localAccountStore: fixture.store)
        let pubkey = try await first.addAccount(secretKey: secretOne)
        try first.setActiveAccount(pubkey)
        XCTAssertThrowsError(try NMPEngine.resetPersistentStore(at: database.path)) { error in
            guard case .storeStillOpen = error as? NMPError else {
                return XCTFail("live store reset must remain a typed refusal: \(error)")
            }
        }
        XCTAssertTrue(
            FileManager.default.fileExists(atPath: database.path),
            "typed live-store refusal must leave the database file intact"
        )
        first.shutdown()

        try NMPEngine.resetPersistentStore(at: database.path)
        XCTAssertFalse(FileManager.default.fileExists(atPath: database.path))
        XCTAssertTrue(FileManager.default.fileExists(atPath: fixture.checkpoint.path))
        try NMPEngine.resetPersistentStore(at: database.path)

        let restored = try NMPEngine(config: config, localAccountStore: fixture.store)
        XCTAssertEqual(try restored.activeAccount(), publicOne)
        restored.shutdown()
    }

    private func makeFixture() throws -> (
        root: URL,
        checkpoint: URL,
        store: NMPInsecureFileAccountStore
    ) {
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString, isDirectory: true)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        let checkpoint = root.appendingPathComponent("local-account.nsec")
        return (
            root,
            checkpoint,
            NMPInsecureFileAccountStore(fileURL: checkpoint)
        )
    }
}
