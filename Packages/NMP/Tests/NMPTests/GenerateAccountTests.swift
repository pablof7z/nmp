import Foundation
import XCTest
@testable import NMP

/// #588: `NMPEngine.generateAccount()` -- the NMP-owned keygen door for a
/// clean-start client, composed from one keygen-only FFI call plus the
/// existing `addAccount(secretKey:)`. Its atomicity/checkpoint/removal
/// behavior is inherited wholesale from `addAccount`'s own suite
/// (`InsecureFileAccountStoreTests.swift`); these tests exercise the
/// composition itself: freshness, non-activation, restart-restore round
/// trip, removal, and checkpoint-failure atomicity through this entry point.
final class GenerateAccountTests: XCTestCase {
    private enum CheckpointFailure: Error, Equatable {
        case injected
    }

    /// Mirrors `InsecureFileAccountStoreTests.FailOnceCheckpoint`: fails the
    /// first `saveSecretKey` so the composed `generateAccount()` call
    /// exercises `addAccount`'s inherited rollback choreography.
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

    func testGenerateAccountCreatesFreshDistinctAccountsAndDoesNotActivate() async throws {
        let engine = try NMPEngine(config: NMPConfig(maxAuthCapabilities: 4))
        defer { engine.shutdown() }

        let first = try await engine.generateAccount()
        let second = try await engine.generateAccount()

        XCTAssertNotEqual(first.publicKey, second.publicKey, "each generated account must be fresh")
        XCTAssertEqual(first.publicKey.count, 64, "public key must be a 32-byte hex string")
        XCTAssertNil(
            try engine.activeAccount(),
            "generateAccount must not activate the account, mirroring addAccount"
        )
    }

    func testGenerateAccountRestartRestoreRoundTrip() async throws {
        let fixture = try makeFixture()
        defer { try? FileManager.default.removeItem(at: fixture.root) }

        let first = try NMPEngine(config: NMPConfig(), localAccountStore: fixture.store)
        let registration = try await first.generateAccount()
        try first.setActiveAccount(registration.publicKey)
        XCTAssertEqual(try first.activeAccount(), registration.publicKey)
        first.shutdown()

        let restored = try NMPEngine(config: NMPConfig(), localAccountStore: fixture.store)
        XCTAssertEqual(
            try restored.activeAccount(),
            registration.publicKey,
            "a generated account checkpoints and restores exactly like addAccount"
        )
        restored.shutdown()
    }

    func testGenerateAccountRegistrationCanBeRemoved() async throws {
        let engine = try NMPEngine(config: NMPConfig(maxAuthCapabilities: 4))
        defer { engine.shutdown() }

        let registration = try await engine.generateAccount()
        try engine.setActiveAccount(registration.publicKey)
        _ = try await engine.signEvent(testEvent())

        XCTAssertTrue(try engine.removeAccount(registration))
        do {
            _ = try await engine.signEvent(testEvent())
            XCTFail("removed account must leave no active signer")
        } catch {
            XCTAssertEqual(error as? NMPError, .noActiveSigner)
        }
        XCTAssertFalse(try engine.removeAccount(registration), "repeated removal must be stale-safe")
    }

    func testGenerateAccountCheckpointFailureRollsBackAndRetrySucceeds() async throws {
        let checkpoint = FailOnceCheckpoint()
        let engine = try NMPEngine(
            config: NMPConfig(maxAuthCapabilities: 1),
            localAccountCheckpoint: checkpoint
        )
        defer { engine.shutdown() }

        do {
            _ = try await engine.generateAccount()
            XCTFail("injected checkpoint failure must escape")
        } catch {
            XCTAssertEqual(error as? CheckpointFailure, .injected)
        }
        XCTAssertNil(try checkpoint.loadSecretKey())

        let registration = try await engine.generateAccount()
        try engine.setActiveAccount(registration.publicKey)
        _ = try await engine.signEvent(testEvent())
        XCTAssertEqual(try checkpoint.loadSecretKey()?.count, 64)
    }

    private func testEvent() -> NMPUnsignedEvent {
        NMPUnsignedEvent(createdAt: 1, kind: 1, tags: [], content: "generated account lifecycle")
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
