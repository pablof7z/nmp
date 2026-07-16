import Foundation
import Security

/// The recommended secure local account checkpoint: a drop-in replacement
/// for `NMPInsecureFileAccountStore` that persists the checkpointed secret
/// key in the platform Keychain (`Security` framework `SecItem` calls)
/// instead of a plaintext app-sandbox file.
///
/// This type implements the exact same internal checkpoint contract as
/// `NMPInsecureFileAccountStore` (`loadSecretKey` / `saveSecretKey` / `clear`
/// -- see `NMPLocalAccountCheckpoint`): `loadSecretKey` restores whatever is
/// currently checkpointed (used on `NMPEngine` construction), `saveSecretKey`
/// checkpoints a newly added account, and `clear` removes the checkpoint
/// (used on sign-out / account removal). It never caches the secret in
/// process memory across calls -- every operation round-trips through the
/// Keychain itself, exactly as the insecure store round-trips through its
/// file, so the two are interchangeable from the caller's point of view.
///
/// Nothing here ever logs, prints, or otherwise surfaces the secret key
/// itself. Failures are reported as a typed `NMPKeychainAccountStoreError`
/// that carries only the Keychain `OSStatus` (or a decode-failure marker),
/// never the value that was being read or written.
public final class NMPKeychainAccountStore: NMPLocalAccountCheckpoint, @unchecked Sendable {
    /// The default `kSecAttrService` bucket used when an app does not need
    /// more than one independent checkpoint. NMP checkpoints exactly one
    /// local account at a time (mirrors `NMPInsecureFileAccountStore`'s
    /// single-file design), so most apps never need to change this.
    public static let defaultService = "app.nmp.localAccount"

    /// The default `kSecAttrAccount` label paired with `defaultService`.
    public static let defaultAccount = "default"

    private let service: String
    private let account: String
    private let accessibility: CFString
    private let lock = NSLock()

    /// - Parameters:
    ///   - service: The Keychain `kSecAttrService` bucket for this
    ///     checkpoint. Defaults to `defaultService`; pass a distinct value
    ///     only if the app deliberately keeps multiple independent
    ///     checkpoints side by side (e.g. more than one signed-in profile).
    ///   - account: The Keychain `kSecAttrAccount` label. Defaults to
    ///     `defaultAccount`.
    ///   - accessibility: The `kSecAttrAccessible` policy applied when the
    ///     item is first created. Defaults to
    ///     `kSecAttrAccessibleAfterFirstThisDeviceOnly`: unreadable until
    ///     the device has been unlocked at least once since boot, never
    ///     synced by iCloud Keychain to another device, and excluded from
    ///     encrypted device-to-device migrations/backups -- appropriate for
    ///     a locally checkpointed signing key that must not silently
    ///     resurrect on a different device.
    public init(
        service: String = NMPKeychainAccountStore.defaultService,
        account: String = NMPKeychainAccountStore.defaultAccount,
        accessibility: CFString = kSecAttrAccessibleAfterFirstThisDeviceOnly
    ) {
        self.service = service
        self.account = account
        self.accessibility = accessibility
    }

    func loadSecretKey() throws -> String? {
        try locked {
            var query = baseQuery()
            query[kSecReturnData as String] = true
            query[kSecMatchLimit as String] = kSecMatchLimitOne

            var result: CFTypeRef?
            let status = SecItemCopyMatching(query as CFDictionary, &result)
            switch status {
            case errSecSuccess:
                guard
                    let data = result as? Data,
                    let secretKey = String(data: data, encoding: .utf8)
                else {
                    throw NMPKeychainAccountStoreError.invalidEncoding
                }
                return secretKey
            case errSecItemNotFound:
                return nil
            default:
                throw NMPKeychainAccountStoreError.keychainStatus(status)
            }
        }
    }

    func saveSecretKey(_ secretKey: String) throws {
        try locked {
            let data = Data(secretKey.utf8)

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
                    throw NMPKeychainAccountStoreError.keychainStatus(addStatus)
                }
            default:
                throw NMPKeychainAccountStoreError.keychainStatus(updateStatus)
            }
        }
    }

    func clear() throws {
        try locked {
            let status = SecItemDelete(baseQuery() as CFDictionary)
            guard status == errSecSuccess || status == errSecItemNotFound else {
                throw NMPKeychainAccountStoreError.keychainStatus(status)
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
            // Opt into the unified data-protection Keychain (macOS 10.15+)
            // rather than the legacy file-based one, matching iOS's single
            // Keychain and avoiding legacy-keychain-specific unlock prompts.
            kSecUseDataProtectionKeychain as String: true,
        ]
    }

    private func locked<T>(_ operation: () throws -> T) rethrows -> T {
        lock.lock()
        defer { lock.unlock() }
        return try operation()
    }
}

/// A Keychain operation on `NMPKeychainAccountStore` did not succeed. Never
/// carries the secret key itself -- only the platform status (or a
/// decode-failure marker), so this is always safe to log.
public enum NMPKeychainAccountStoreError: Error, Equatable {
    /// A `SecItem*` call returned this non-success `OSStatus`.
    case keychainStatus(OSStatus)
    /// The Keychain returned a value that was not valid UTF-8 text.
    case invalidEncoding
}
