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
