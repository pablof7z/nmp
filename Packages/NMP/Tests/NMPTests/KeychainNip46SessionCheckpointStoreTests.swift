import Foundation
import Security
import XCTest
@testable import NMP

/// Mirrors `KeychainAccountStoreTests`'s save -> restore -> clear round
/// trip and environment-unavailable skip discipline, but against
/// `NMPKeychainNip46SessionCheckpointStore`'s real Keychain-backed NIP-46
/// session checkpoint (#571).
final class KeychainNip46SessionCheckpointStoreTests: XCTestCase {
    private static let environmentUnavailableStatuses: Set<OSStatus> = [
        errSecInteractionNotAllowed,
        errSecNotAvailable,
        errSecMissingEntitlement,
    ]

    private let checkpointOne = NMPNip46SessionCheckpoint(
        clientSecretKey: String(repeating: "1", count: 64),
        userPublicKey: String(repeating: "2", count: 64),
        remoteSignerPublicKey: String(repeating: "3", count: 64),
        relays: ["wss://relay.example"],
        origin: .clientInitiated
    )

    func testCheckpointSaveRestoresValueAndClearReturnsToEmpty() throws {
        let store = makeStore()
        defer { try? store.clear() }

        try runOrSkipIfKeychainUnavailable { try store.saveCheckpoint(checkpointOne) }

        let restored = try runOrSkipIfKeychainUnavailable { try store.loadCheckpoint() }
        XCTAssertEqual(restored, checkpointOne)

        try store.clear()
        XCTAssertNil(try store.loadCheckpoint())
    }

    func testLoadCheckpointReturnsNilWhenNothingIsStored() throws {
        let store = makeStore()
        try runOrSkipIfKeychainUnavailable { try store.clear() }

        XCTAssertNil(try store.loadCheckpoint())
    }

    func testSaveCheckpointOverwritesAnExistingCheckpointInPlace() throws {
        let store = makeStore()
        defer { try? store.clear() }
        let checkpointTwo = NMPNip46SessionCheckpoint(
            clientSecretKey: String(repeating: "4", count: 64),
            userPublicKey: String(repeating: "5", count: 64),
            remoteSignerPublicKey: String(repeating: "6", count: 64),
            relays: ["wss://relay2.example"],
            origin: .bunker
        )

        try runOrSkipIfKeychainUnavailable { try store.saveCheckpoint(checkpointOne) }
        try runOrSkipIfKeychainUnavailable { try store.saveCheckpoint(checkpointTwo) }

        XCTAssertEqual(try store.loadCheckpoint(), checkpointTwo)
    }

    func testClearIsIdempotentWhenNothingIsStored() throws {
        let store = makeStore()
        try runOrSkipIfKeychainUnavailable { try store.clear() }
        try store.clear()
        XCTAssertNil(try store.loadCheckpoint())
    }

    // MARK: - Helpers

    private func makeStore() -> NMPKeychainNip46SessionCheckpointStore {
        NMPKeychainNip46SessionCheckpointStore(
            service: "org.nmp.tests.keychainNip46SessionCheckpointStore",
            account: "test-\(UUID().uuidString)"
        )
    }

    @discardableResult
    private func runOrSkipIfKeychainUnavailable<T>(
        file: StaticString = #filePath,
        line: UInt = #line,
        _ operation: () throws -> T
    ) throws -> T {
        do {
            return try operation()
        } catch NMPKeychainNip46SessionCheckpointStoreError.keychainStatus(let status)
        where Self.environmentUnavailableStatuses.contains(status) {
            throw XCTSkip(
                "Keychain unavailable in this environment (OSStatus \(status)) -- "
                    + "package build still proves the SecItem call sites compile and "
                    + "link correctly against the platform SDK.",
                file: file,
                line: line
            )
        }
    }
}
