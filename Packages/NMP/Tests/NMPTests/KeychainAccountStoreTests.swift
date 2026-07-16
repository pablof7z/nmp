import Foundation
import Security
import XCTest
@testable import NMP

/// Mirrors `InsecureFileAccountStoreTests`'s save -> restore -> clear
/// round-trip, but against `NMPKeychainAccountStore`'s real Keychain-backed
/// checkpoint instead of a plaintext file.
///
/// Unlike the file store, a real Keychain is not guaranteed reachable from
/// every CI sandbox -- a headless runner may have no unlocked login /
/// data-protection keychain, or may run the XCTest bundle without the
/// entitlement a Keychain item write would otherwise require. Those
/// environment gaps surface as one of a small set of well-known `OSStatus`
/// codes; this test treats only those as "Keychain unavailable here" and
/// skips, so it still fails on an actual store bug (wrong query, wrong
/// item lifecycle, secret corruption, etc).
final class KeychainAccountStoreTests: XCTestCase {
    private let secretOne = String(repeating: "0", count: 63) + "1"

    /// `OSStatus` values that mean "this environment has no usable
    /// Keychain for this process" rather than "the store is broken".
    private static let environmentUnavailableStatuses: Set<OSStatus> = [
        errSecInteractionNotAllowed,
        errSecNotAvailable,
        errSecMissingEntitlement,
    ]

    func testCheckpointSaveRestoresSecretAndClearReturnsToReadOnly() throws {
        let store = makeStore()
        defer { try? store.clear() }

        try runOrSkipIfKeychainUnavailable { try store.saveSecretKey(secretOne) }

        let restored = try runOrSkipIfKeychainUnavailable { try store.loadSecretKey() }
        XCTAssertEqual(restored, secretOne)

        try store.clear()
        XCTAssertNil(try store.loadSecretKey())
    }

    func testLoadSecretKeyReturnsNilWhenNothingIsCheckpointed() throws {
        let store = makeStore()
        try store.clear()

        XCTAssertNil(try store.loadSecretKey())
    }

    func testSaveSecretKeyOverwritesAnExistingCheckpointInPlace() throws {
        let store = makeStore()
        defer { try? store.clear() }
        let secretTwo = String(repeating: "1", count: 63) + "0"

        try runOrSkipIfKeychainUnavailable { try store.saveSecretKey(secretOne) }
        try runOrSkipIfKeychainUnavailable { try store.saveSecretKey(secretTwo) }

        XCTAssertEqual(try store.loadSecretKey(), secretTwo)
    }

    func testClearIsIdempotentWhenNothingIsCheckpointed() throws {
        let store = makeStore()
        try store.clear()
        try store.clear()
        XCTAssertNil(try store.loadSecretKey())
    }

    // MARK: - Helpers

    /// A fresh service/account pair per test so parallel test runs (and
    /// reruns after a failure) never collide on the same Keychain item.
    private func makeStore() -> NMPKeychainAccountStore {
        NMPKeychainAccountStore(
            service: "org.nmp.tests.keychainAccountStore",
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
        } catch NMPKeychainAccountStoreError.keychainStatus(let status)
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
