import Foundation
import XCTest
@testable import NMP

final class InsecureFileAccountStoreTests: XCTestCase {
    private enum CheckpointFailure: Error, Equatable {
        case injected
    }

    private enum RollbackFailure: Error, Equatable {
        case injected
    }

    private final class FailOnceCheckpoint: NMPLocalAccountCheckpoint, @unchecked Sendable {
        private let lock = NSLock()
        private var shouldFail = true
        private var secretKey: String?

        func loadSecretKey() throws -> String? {
            lock.withLock { secretKey }
        }

        func saveSecretKey(_ secretKey: String) throws {
            try lock.withLock {
                if shouldFail {
                    shouldFail = false
                    throw CheckpointFailure.injected
                }
                self.secretKey = secretKey
            }
        }

        func clear() throws {
            lock.withLock { secretKey = nil }
        }
    }

    /// A NON-SDK conformer: the minimal store an app (or a secure vault
    /// provider) would write itself, proving `NMPLocalAccountCheckpoint` is
    /// a genuinely public seam, not a blessed-concrete-type parameter.
    private final class InMemoryCheckpoint: NMPLocalAccountCheckpoint, @unchecked Sendable {
        private let lock = NSLock()
        private var secretKey: String?

        init(secretKey: String? = nil) {
            self.secretKey = secretKey
        }

        func loadSecretKey() throws -> String? {
            lock.withLock { secretKey }
        }

        func saveSecretKey(_ secretKey: String) throws {
            lock.withLock { self.secretKey = secretKey }
        }

        func clear() throws {
            lock.withLock { secretKey = nil }
        }
    }

    private let secretOne = String(repeating: "0", count: 63) + "1"
    private let publicOne = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"

    /// Any conforming checkpoint store is a drop-in through the PUBLIC
    /// `init(config:localAccountStore:)`: a custom conformer's
    /// `loadSecretKey` drives the restore exactly like the SDK's own file
    /// store, and `addAccount` checkpoints back into it.
    func testCustomCheckpointConformerRestoresThroughPublicInit() async throws {
        let seeded = InMemoryCheckpoint(secretKey: secretOne)
        let restored = try NMPEngine(config: NMPConfig(), localAccountStore: seeded)
        XCTAssertEqual(try restored.activeAccount(), publicOne)
        restored.shutdown()

        let empty = InMemoryCheckpoint()
        let engine = try NMPEngine(config: NMPConfig(), localAccountStore: empty)
        XCTAssertNil(try engine.activeAccount())
        _ = try await engine.addAccount(secretKey: secretOne)
        XCTAssertEqual(try empty.loadSecretKey(), secretOne)
        engine.shutdown()
    }

    func testCheckpointRestoresActiveAccountAndClearReturnsToReadOnly() async throws {
        let fixture = try makeFixture()
        defer { try? FileManager.default.removeItem(at: fixture.root) }

        let first = try NMPEngine(
            config: NMPConfig(),
            localAccountStore: fixture.store
        )
        let registration = try await first.addAccount(secretKey: secretOne)
        try first.setActiveAccount(registration.publicKey)
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
        let registration = try await first.addAccount(secretKey: secretOne)
        try first.setActiveAccount(registration.publicKey)
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

    func testCheckpointFailureRollsBackExactLiveSignerAndPreservesOriginalError() async throws {
        let checkpoint = FailOnceCheckpoint()
        let engine = try NMPEngine(
            config: NMPConfig(maxAuthCapabilities: 1),
            localAccountCheckpoint: checkpoint
        )
        defer { engine.shutdown() }

        do {
            _ = try await engine.addAccount(secretKey: secretOne)
            XCTFail("injected checkpoint failure must escape")
        } catch {
            XCTAssertEqual(error as? CheckpointFailure, .injected)
        }

        try engine.setActiveAccount(publicOne)
        await assertNoActiveSigner(engine)

        let registration = try await engine.addAccount(secretKey: secretOne)
        XCTAssertEqual(registration.publicKey, publicOne)
        _ = try await engine.signEvent(testEvent())
        XCTAssertTrue(try engine.removeAccount(registration))
        await assertNoActiveSigner(engine)
        XCTAssertFalse(try engine.removeAccount(registration))
    }

    func testAccountRegistrationRemovalIsStaleSafeForSameKeyReplacement() async throws {
        let engine = try NMPEngine(config: NMPConfig(maxAuthCapabilities: 1))
        defer { engine.shutdown() }

        let first = try await engine.addAccount(secretKey: secretOne)
        let replacement = try await engine.addAccount(secretKey: secretOne)
        XCTAssertEqual(first.publicKey, replacement.publicKey)
        XCTAssertFalse(try engine.removeAccount(first))

        try engine.setActiveAccount(replacement.publicKey)
        _ = try await engine.signEvent(testEvent())
        XCTAssertTrue(try engine.removeAccount(replacement))
        await assertNoActiveSigner(engine)
        XCTAssertFalse(try engine.removeAccount(replacement))
    }

    /// #529: removing the checkpointed account through its registration must
    /// also clear the on-disk checkpoint, so the removed account cannot
    /// resurrect through the restore path of the next engine.
    func testRemoveAccountClearsCheckpointSoRemovedAccountCannotResurrect() async throws {
        let fixture = try makeFixture()
        defer { try? FileManager.default.removeItem(at: fixture.root) }

        let engine = try NMPEngine(
            config: NMPConfig(),
            localAccountStore: fixture.store
        )
        let registration = try await engine.addAccount(secretKey: secretOne)
        try engine.setActiveAccount(registration.publicKey)
        XCTAssertTrue(FileManager.default.fileExists(atPath: fixture.checkpoint.path))

        XCTAssertTrue(try engine.removeAccount(registration))
        XCTAssertFalse(
            FileManager.default.fileExists(atPath: fixture.checkpoint.path),
            "removing the checkpointed account must clear its on-disk checkpoint"
        )
        engine.shutdown()

        let restarted = try NMPEngine(
            config: NMPConfig(),
            localAccountStore: fixture.store
        )
        XCTAssertNil(
            try restarted.activeAccount(),
            "a removed account must not resurrect on the next restart"
        )
        restarted.shutdown()
    }

    /// #529: a stale registration removal returns `false` and must leave the
    /// checkpoint intact -- only the exact live installation may clear it.
    func testStaleRegistrationRemovalLeavesCheckpointIntact() async throws {
        let fixture = try makeFixture()
        defer { try? FileManager.default.removeItem(at: fixture.root) }

        let engine = try NMPEngine(
            config: NMPConfig(maxAuthCapabilities: 1),
            localAccountStore: fixture.store
        )
        let first = try await engine.addAccount(secretKey: secretOne)
        let replacement = try await engine.addAccount(secretKey: secretOne)
        XCTAssertEqual(first.publicKey, replacement.publicKey)

        XCTAssertFalse(try engine.removeAccount(first))
        XCTAssertTrue(
            FileManager.default.fileExists(atPath: fixture.checkpoint.path),
            "a stale removal must not touch the checkpoint"
        )
        engine.shutdown()

        let restored = try NMPEngine(
            config: NMPConfig(),
            localAccountStore: fixture.store
        )
        XCTAssertEqual(try restored.activeAccount(), publicOne)
        restored.shutdown()
    }

    func testCheckpointRollbackFailureAttachesContextWithoutErasingPersistenceError() {
        XCTAssertThrowsError(
            try rethrowCheckpointFailureAfterRollback(CheckpointFailure.injected) { false }
        ) { error in
            guard let composite = error as? NMPAccountCheckpointRollbackError else {
                return XCTFail("expected checkpoint rollback composite")
            }
            XCTAssertEqual(composite.persistenceError as? CheckpointFailure, .injected)
            guard case .registrationWasNotActive = composite.rollbackFailure else {
                return XCTFail("expected exact-registration false rollback context")
            }
        }

        XCTAssertThrowsError(
            try rethrowCheckpointFailureAfterRollback(CheckpointFailure.injected) {
                throw RollbackFailure.injected
            }
        ) { error in
            guard let composite = error as? NMPAccountCheckpointRollbackError else {
                return XCTFail("expected checkpoint rollback composite")
            }
            XCTAssertEqual(composite.persistenceError as? CheckpointFailure, .injected)
            guard case .removalFailed(let rollbackError) = composite.rollbackFailure else {
                return XCTFail("expected thrown rollback context")
            }
            XCTAssertEqual(rollbackError as? RollbackFailure, .injected)
        }
    }

    func testSuccessfulCheckpointRollbackRethrowsOriginalErrorDirectly() {
        XCTAssertThrowsError(
            try rethrowCheckpointFailureAfterRollback(CheckpointFailure.injected) { true }
        ) { error in
            XCTAssertEqual(error as? CheckpointFailure, .injected)
            XCTAssertFalse(error is NMPAccountCheckpointRollbackError)
        }
    }

    private func assertNoActiveSigner(
        _ engine: NMPEngine,
        file: StaticString = #filePath,
        line: UInt = #line
    ) async {
        do {
            _ = try await engine.signEvent(testEvent())
            XCTFail("removed account must leave no active signer", file: file, line: line)
        } catch {
            XCTAssertEqual(error as? NMPError, .noActiveSigner, file: file, line: line)
        }
    }

    private func testEvent() -> NMPUnsignedEvent {
        NMPUnsignedEvent(createdAt: 1, kind: 1, tags: [], content: "account lifecycle")
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
