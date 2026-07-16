import Foundation

/// A pluggable local account checkpoint -- the seam `NMPEngine` uses to
/// restore the checkpointed account at construction and to persist one on
/// `addAccount`. ANY conformer is a drop-in through the public
/// `NMPEngine(config:localAccountStore:)` init: the platform-vault
/// providers (Keychain / Secure Enclave), an app's own store, or the
/// plaintext-file convenience below.
///
/// A checkpoint hands the raw secret key to its holder by design -- the
/// engine needs it for account registration, and the app owns its keys
/// (#47): import, removal, backup, and consent are app policy, never SDK
/// policy. The recommended secure providers keep material in the platform
/// vault; `NMPInsecureFileAccountStore` remains the explicit
/// convenience-over-security option.
public protocol NMPLocalAccountCheckpoint: Sendable {
    /// The checkpointed secret key (hex or bech32 `nsec`), or `nil` when no
    /// account is checkpointed.
    func loadSecretKey() throws -> String?
    /// Persist `secretKey` as the one checkpointed account, replacing any
    /// previous checkpoint.
    func saveSecretKey(_ secretKey: String) throws
    /// Remove the checkpoint. A later `loadSecretKey` returns `nil`.
    func clear() throws
}

/// An explicit convenience-over-security local account checkpoint.
///
/// The secret is stored as plaintext UTF-8 in the caller-selected app-sandbox
/// file. This type does not use Keychain, Secure Enclave, encryption, or
/// hardware-backed protection -- and like every `NMPLocalAccountCheckpoint`
/// its methods hand the raw secret to whoever holds the store (see the
/// protocol's doc for why that is the design, not a leak). Prefer a
/// platform-vault conformer; this store exists for the explicit
/// convenience-over-security call.
public final class NMPInsecureFileAccountStore: NMPLocalAccountCheckpoint, @unchecked Sendable {
    private let fileURL: URL
    private let lock = NSLock()

    public init(fileURL: URL) {
        self.fileURL = fileURL
    }

    public func loadSecretKey() throws -> String? {
        try locked {
            guard FileManager.default.fileExists(atPath: fileURL.path) else {
                return nil
            }
            let data = try Data(contentsOf: fileURL)
            guard let secretKey = String(data: data, encoding: .utf8) else {
                throw NSError(
                    domain: NSCocoaErrorDomain,
                    code: NSFileReadInapplicableStringEncodingError
                )
            }
            return secretKey
        }
    }

    public func saveSecretKey(_ secretKey: String) throws {
        try locked {
            let directory = fileURL.deletingLastPathComponent()
            try FileManager.default.createDirectory(
                at: directory,
                withIntermediateDirectories: true,
                attributes: [.posixPermissions: 0o700]
            )
            try Data(secretKey.utf8).write(to: fileURL, options: .atomic)
            do {
                try FileManager.default.setAttributes(
                    [.posixPermissions: 0o600],
                    ofItemAtPath: fileURL.path
                )
            } catch {
                try? FileManager.default.removeItem(at: fileURL)
                throw error
            }
        }
    }

    public func clear() throws {
        try locked {
            guard FileManager.default.fileExists(atPath: fileURL.path) else {
                return
            }
            try FileManager.default.removeItem(at: fileURL)
        }
    }

    private func locked<T>(_ operation: () throws -> T) rethrows -> T {
        lock.lock()
        defer { lock.unlock() }
        return try operation()
    }
}
