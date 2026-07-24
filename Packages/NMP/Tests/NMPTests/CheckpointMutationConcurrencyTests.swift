import Dispatch
import Foundation
import XCTest
@testable import NMP

/// #607: checkpoint bytes and the SDK's checkpoint metadata are one
/// serialized mutation domain. These barriers hold a destructive store
/// operation open while a successful account add tries to replace it,
/// deterministically exercising the old lost-checkpoint interleaving.
final class CheckpointMutationConcurrencyTests: XCTestCase {
    private final class BlockingClearCheckpoint:
        NMPLocalAccountCheckpoint,
        @unchecked Sendable
    {
        private let stateLock = NSLock()
        private let blockLock = NSLock()
        private var secretKey: String?
        private var shouldBlockNextClear = true
        private let clearEntered = DispatchSemaphore(value: 0)
        private let clearRelease = DispatchSemaphore(value: 0)
        private let saveEntered = DispatchSemaphore(value: 0)

        init(secretKey: String? = nil) {
            self.secretKey = secretKey
        }

        func loadSecretKey() throws -> String? {
            stateLock.withLock { secretKey }
        }

        func saveSecretKey(_ secretKey: String) throws {
            saveEntered.signal()
            stateLock.withLock {
                self.secretKey = secretKey
            }
        }

        func clear() throws {
            let shouldBlock = blockLock.withLock {
                defer { shouldBlockNextClear = false }
                return shouldBlockNextClear
            }
            if shouldBlock {
                clearEntered.signal()
                _ = clearRelease.wait(timeout: .now() + 5)
            }
            stateLock.withLock {
                secretKey = nil
            }
        }

        func waitUntilClearEntered() -> DispatchTimeoutResult {
            clearEntered.wait(timeout: .now() + 5)
        }

        func waitForConcurrentSave() -> DispatchTimeoutResult {
            saveEntered.wait(timeout: .now() + 1)
        }

        func releaseClear() {
            clearRelease.signal()
        }
    }

    private let secretOne = String(repeating: "0", count: 63) + "1"
    private let secretTwo = String(repeating: "0", count: 63) + "2"

    private func assertRestartRestoresAndDetaches(
        _ publicKey: String,
        from checkpoint: BlockingClearCheckpoint
    ) throws {
        let restarted = try NMPEngine(
            config: NMPConfig(),
            localAccountCheckpoint: checkpoint
        )
        defer { restarted.shutdown() }

        XCTAssertEqual(try restarted.activeAccount(), publicKey)
        XCTAssertTrue(try restarted.detachPersistedAccount())
        XCTAssertNil(
            try checkpoint.loadSecretKey(),
            "restart metadata must describe the same checkpoint material that was restored"
        )
    }

    func testAddWaitsForConcurrentCheckpointClearAndPersistsTheNewAccount() async throws {
        let checkpoint = BlockingClearCheckpoint(secretKey: secretOne)
        let replacementSecret = secretTwo
        let engine = try NMPEngine(
            config: NMPConfig(),
            localAccountCheckpoint: checkpoint
        )
        defer { engine.shutdown() }

        let clearTask = Task.detached {
            try engine.clearPersistedAccount()
        }
        XCTAssertEqual(checkpoint.waitUntilClearEntered(), .success)

        let addStarted = DispatchSemaphore(value: 0)
        let addTask = Task.detached {
            addStarted.signal()
            return try await engine.addAccount(secretKey: replacementSecret)
        }
        XCTAssertEqual(addStarted.wait(timeout: .now() + 5), .success)
        XCTAssertEqual(
            checkpoint.waitForConcurrentSave(),
            .timedOut,
            "save must not enter the checkpoint store while clear owns the mutation domain"
        )

        checkpoint.releaseClear()
        try await clearTask.value
        let registration = try await addTask.value

        XCTAssertEqual(try checkpoint.loadSecretKey(), replacementSecret)
        engine.shutdown()
        try assertRestartRestoresAndDetaches(registration.publicKey, from: checkpoint)
    }

    func testAddWaitsForConcurrentAccountRemovalAndPersistsTheNewAccount() async throws {
        let checkpoint = BlockingClearCheckpoint()
        let originalSecret = secretOne
        let replacementSecret = secretTwo
        let engine = try NMPEngine(
            config: NMPConfig(),
            localAccountCheckpoint: checkpoint
        )
        defer { engine.shutdown() }

        let original = try await engine.addAccount(secretKey: originalSecret)
        XCTAssertEqual(checkpoint.waitForConcurrentSave(), .success)

        let removeTask = Task.detached {
            try engine.removeAccount(original)
        }
        XCTAssertEqual(checkpoint.waitUntilClearEntered(), .success)

        let addStarted = DispatchSemaphore(value: 0)
        let addTask = Task.detached {
            addStarted.signal()
            return try await engine.addAccount(secretKey: replacementSecret)
        }
        XCTAssertEqual(addStarted.wait(timeout: .now() + 5), .success)
        XCTAssertEqual(
            checkpoint.waitForConcurrentSave(),
            .timedOut,
            "save must not enter the checkpoint store while remove owns the mutation domain"
        )

        checkpoint.releaseClear()
        let removed = try await removeTask.value
        XCTAssertTrue(removed)
        let registration = try await addTask.value

        XCTAssertEqual(try checkpoint.loadSecretKey(), replacementSecret)
        engine.shutdown()
        try assertRestartRestoresAndDetaches(registration.publicKey, from: checkpoint)
    }
}
