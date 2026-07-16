import Foundation
import Security

/// The recommended secure NIP-46 session checkpoint store (#571): persists
/// `NMPNip46SessionCheckpoint.serialize()`'s versioned blob in the platform
/// Keychain (`Security` framework `SecItem` calls), mirroring
/// `NMPKeychainAccountStore`'s exact shape and precedent one file over.
///
/// Every operation round-trips through the Keychain itself -- nothing here
/// ever caches the checkpoint in process memory across calls, and nothing
/// here ever logs, prints, or otherwise surfaces the secret. Failures are
/// reported as a typed `NMPKeychainNip46SessionCheckpointStoreError` that
/// carries only the Keychain `OSStatus` (or a decode-failure marker), never
/// the value being read or written.
public final class NMPKeychainNip46SessionCheckpointStore: NMPNip46SessionCheckpointStore, @unchecked Sendable {
    /// The default `kSecAttrService` bucket used when an app does not need
    /// more than one independent NIP-46 session checkpoint.
    public static let defaultService = "app.nmp.nip46Session"

    /// The default `kSecAttrAccount` label paired with `defaultService`.
    public static let defaultAccount = "default"

    private let service: String
    private let account: String
    private let accessibility: CFString
    private let lock = NSLock()

    /// - Parameters:
    ///   - service: The Keychain `kSecAttrService` bucket for this
    ///     checkpoint. Defaults to `defaultService`; pass a distinct value
    ///     only if the app deliberately keeps multiple independent NIP-46
    ///     session checkpoints side by side.
    ///   - account: The Keychain `kSecAttrAccount` label. Defaults to
    ///     `defaultAccount`.
    ///   - accessibility: The `kSecAttrAccessible` policy applied when the
    ///     item is first created. Defaults to
    ///     `kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly`, matching
    ///     `NMPKeychainAccountStore`'s own default and rationale.
    public init(
        service: String = NMPKeychainNip46SessionCheckpointStore.defaultService,
        account: String = NMPKeychainNip46SessionCheckpointStore.defaultAccount,
        accessibility: CFString = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
    ) {
        self.service = service
        self.account = account
        self.accessibility = accessibility
    }

    public func loadCheckpoint() throws -> NMPNip46SessionCheckpoint? {
        try locked {
            var query = baseQuery()
            query[kSecReturnData as String] = true
            query[kSecMatchLimit as String] = kSecMatchLimitOne

            var result: CFTypeRef?
            let status = SecItemCopyMatching(query as CFDictionary, &result)
            switch status {
            case errSecSuccess:
                guard let data = result as? Data else {
                    throw NMPKeychainNip46SessionCheckpointStoreError.invalidEncoding
                }
                do {
                    return try NMPNip46SessionCheckpoint.deserialize(data)
                } catch {
                    throw NMPKeychainNip46SessionCheckpointStoreError.invalidEncoding
                }
            case errSecItemNotFound:
                return nil
            default:
                throw NMPKeychainNip46SessionCheckpointStoreError.keychainStatus(status)
            }
        }
    }

    public func saveCheckpoint(_ checkpoint: NMPNip46SessionCheckpoint) throws {
        try locked {
            let data: Data
            do {
                data = try checkpoint.serialize()
            } catch {
                throw NMPKeychainNip46SessionCheckpointStoreError.invalidEncoding
            }

            let updateStatus = SecItemUpdate(
                baseQuery() as CFDictionary,
                [kSecValueData as String: data] as CFDictionary
            )
            switch updateStatus {
            case errSecSuccess:
                return
            case errSecItemNotFound:
                var addQuery = baseQuery()
                addQuery[kSecValueData as String] = data
                addQuery[kSecAttrAccessible as String] = accessibility
                let addStatus = SecItemAdd(addQuery as CFDictionary, nil)
                guard addStatus == errSecSuccess else {
                    throw NMPKeychainNip46SessionCheckpointStoreError.keychainStatus(addStatus)
                }
            default:
                throw NMPKeychainNip46SessionCheckpointStoreError.keychainStatus(updateStatus)
            }
        }
    }

    public func clear() throws {
        try locked {
            let status = SecItemDelete(baseQuery() as CFDictionary)
            guard status == errSecSuccess || status == errSecItemNotFound else {
                throw NMPKeychainNip46SessionCheckpointStoreError.keychainStatus(status)
            }
        }
    }

    /// The identity portion of the query, shared by copy/update/delete so
    /// every operation addresses exactly one Keychain item. Intentionally
    /// excludes `kSecValueData`/`kSecAttrAccessible`, which only apply when
    /// creating (or reading back) the item.
    private func baseQuery() -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecUseDataProtectionKeychain as String: true,
        ]
    }

    private func locked<T>(_ operation: () throws -> T) rethrows -> T {
        lock.lock()
        defer { lock.unlock() }
        return try operation()
    }
}

/// A Keychain operation on `NMPKeychainNip46SessionCheckpointStore` did not
/// succeed. Never carries the checkpoint itself -- only the platform status
/// (or a decode-failure marker), so this is always safe to log.
public enum NMPKeychainNip46SessionCheckpointStoreError: Error, Equatable {
    /// A `SecItem*` call returned this non-success `OSStatus`.
    case keychainStatus(OSStatus)
    /// The Keychain returned a value that was not a valid serialized
    /// checkpoint.
    case invalidEncoding
}
