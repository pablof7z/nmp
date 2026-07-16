import Foundation
import XCTest
@testable import NMP

/// #589: `detachPersistedAccount()` -- the exact-registration detach for
/// whichever account this engine restored from its configured checkpoint at
/// construction. Wrapper-only: it is `removeAccount(_:)`'s already-durable
/// checkpoint-clear contract, reused verbatim, applied to the init-restore
/// registration `CheckpointTracker` now also retains.
final class DetachPersistedAccountTests: XCTestCase {
    private enum ClearFailure: Error, Equatable {
        case injected
    }

    /// A checkpoint whose `clear()` fails exactly once, to deterministically
    /// exercise `NMPAccountCheckpointClearError`'s documented recovery path
    /// (retry via `clearPersistedAccount()`) without depending on a real
    /// filesystem failure condition.
    private final class FailOnceOnClearCheckpoint: NMPLocalAccountCheckpoint, @unchecked Sendable {
        private let lock = NSLock()
        private var secretKey: String?
        private var shouldFailClear = true

        init(secretKey: String?) {
            self.secretKey = secretKey
        }

        func loadSecretKey() throws -> String? {
            lock.withLock { secretKey }
        }

        func saveSecretKey(_ secretKey: String) throws {
            lock.withLock { self.secretKey = secretKey }
        }

        func clear() throws {
            try lock.withLock {
                if shouldFailClear {
                    shouldFailClear = false
                    throw ClearFailure.injected
                }
                secretKey = nil
            }
        }
    }

    private let secretOne = String(repeating: "0", count: 63) + "1"
    private let secretTwo = String(repeating: "0", count: 63) + "2"
    private let publicOne = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"

    func testColdRestoreThenDetachClearsCheckpointSignerAndDoesNotResurrect() async throws {
        let fixture = try makeFixture()
        defer { try? FileManager.default.removeItem(at: fixture.root) }

        let seed = try NMPEngine(config: NMPConfig(), localAccountStore: fixture.store)
        let registration = try await seed.addAccount(secretKey: secretOne)
        try seed.setActiveAccount(registration.publicKey)
        seed.shutdown()
        XCTAssertTrue(FileManager.default.fileExists(atPath: fixture.checkpoint.path))

        let restored = try NMPEngine(config: NMPConfig(), localAccountStore: fixture.store)
        XCTAssertEqual(try restored.activeAccount(), publicOne)

        XCTAssertTrue(try restored.detachPersistedAccount())
        XCTAssertFalse(
            FileManager.default.fileExists(atPath: fixture.checkpoint.path),
            "detach must clear the on-disk checkpoint like removeAccount does"
        )
        do {
            _ = try await restored.signEvent(testEvent())
            XCTFail("detach must remove the live signer installation")
        } catch {
            XCTAssertEqual(error as? NMPError, .noActiveSigner)
        }
        restored.shutdown()

        let next = try NMPEngine(config: NMPConfig(), localAccountStore: fixture.store)
        XCTAssertNil(
            try next.activeAccount(),
            "a detached account must not resurrect on next launch"
        )
        next.shutdown()
    }

    func testRepeatedDetachReturnsFalse() async throws {
        let fixture = try makeFixture()
        defer { try? FileManager.default.removeItem(at: fixture.root) }

        let seed = try NMPEngine(config: NMPConfig(), localAccountStore: fixture.store)
        let registration = try await seed.addAccount(secretKey: secretOne)
        try seed.setActiveAccount(registration.publicKey)
        seed.shutdown()

        let engine = try NMPEngine(config: NMPConfig(), localAccountStore: fixture.store)
        XCTAssertTrue(try engine.detachPersistedAccount())
        XCTAssertFalse(
            try engine.detachPersistedAccount(),
            "a second detach on an already-spent registration must be a stale-safe no-op"
        )
        engine.shutdown()
    }

    func testDetachWithNoRestoredAccountReturnsFalse() throws {
        let fixture = try makeFixture()
        defer { try? FileManager.default.removeItem(at: fixture.root) }

        // Nothing was ever checkpointed -- construction restores nothing.
        let engine = try NMPEngine(config: NMPConfig(), localAccountStore: fixture.store)
        XCTAssertFalse(try engine.detachPersistedAccount())
        engine.shutdown()

        // No checkpoint store configured at all.
        let bare = try NMPEngine(config: NMPConfig())
        XCTAssertFalse(try bare.detachPersistedAccount())
        bare.shutdown()
    }

    func testDetachAfterLaterAddAccountOverwriteReturnsFalse() async throws {
        let fixture = try makeFixture()
        defer { try? FileManager.default.removeItem(at: fixture.root) }

        let seed = try NMPEngine(config: NMPConfig(), localAccountStore: fixture.store)
        let restoredRegistration = try await seed.addAccount(secretKey: secretOne)
        try seed.setActiveAccount(restoredRegistration.publicKey)
        seed.shutdown()

        let engine = try NMPEngine(config: NMPConfig(), localAccountStore: fixture.store)
        XCTAssertEqual(try engine.activeAccount(), publicOne)

        // A later `addAccount` overwrites the on-disk checkpoint with a
        // different installation; the originally-restored registration is no
        // longer the one the checkpoint holds.
        _ = try await engine.addAccount(secretKey: secretTwo)

        XCTAssertFalse(
            try engine.detachPersistedAccount(),
            "detach must not fire once a later addAccount has overwritten the checkpoint"
        )
        engine.shutdown()
    }

    func testDetachAfterCheckpointClearFailureIsRecoverableViaClearPersistedAccount() throws {
        let checkpoint = FailOnceOnClearCheckpoint(secretKey: secretOne)
        let engine = try NMPEngine(
            config: NMPConfig(),
            localAccountCheckpoint: checkpoint
        )
        defer { engine.shutdown() }
        XCTAssertEqual(try engine.activeAccount(), publicOne)

        do {
            _ = try engine.detachPersistedAccount()
            XCTFail("injected checkpoint-clear failure must escape as a typed error")
        } catch let error as NMPAccountCheckpointClearError {
            XCTAssertEqual(error.underlying as? ClearFailure, .injected)
        }

        // Documented recovery per `removeAccount`'s contract: the engine-side
        // removal already stood (the registration is spent), so a second
        // detach is stale-safe...
        XCTAssertFalse(try engine.detachPersistedAccount())
        // ...and the caller retries the file cleanup directly.
        try engine.clearPersistedAccount()
        XCTAssertNil(try checkpoint.loadSecretKey())
    }

    func testCanonicalStoreAndCachePreservedAcrossDetach() async throws {
        let fixture = try makeFixture()
        defer { try? FileManager.default.removeItem(at: fixture.root) }
        let database = fixture.root.appendingPathComponent("nmp.redb")
        let config = NMPConfig(storePath: database.path)

        let seed = try NMPEngine(config: config, localAccountStore: fixture.store)
        let registration = try await seed.addAccount(secretKey: secretOne)
        try seed.setActiveAccount(registration.publicKey)
        seed.shutdown()

        let restored = try NMPEngine(config: config, localAccountStore: fixture.store)
        XCTAssertTrue(try restored.detachPersistedAccount())
        restored.shutdown()

        XCTAssertTrue(
            FileManager.default.fileExists(atPath: database.path),
            "detach must never touch the canonical NMP store"
        )

        // The canonical store reopens cleanly with no active account -- the
        // detach only removed the signer + checkpoint, never cached data.
        let reopened = try NMPEngine(config: config, localAccountStore: fixture.store)
        XCTAssertNil(try reopened.activeAccount())
        reopened.shutdown()
    }

    private func testEvent() -> NMPUnsignedEvent {
        NMPUnsignedEvent(createdAt: 1, kind: 1, tags: [], content: "detach lifecycle")
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
