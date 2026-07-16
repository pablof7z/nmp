import Foundation

protocol NMPLocalAccountCheckpoint: Sendable {
    func loadSecretKey() throws -> String?
    func saveSecretKey(_ secretKey: String) throws
    func clear() throws
}

/// An explicit convenience-over-security local account checkpoint.
///
/// The secret is stored as plaintext UTF-8 in the caller-selected app-sandbox
/// file. This type does not use Keychain, Secure Enclave, encryption, or
/// hardware-backed protection. Its file operations stay inside the NMP SDK so
/// a consuming app cannot read the secret back through this public surface.
public final class NMPInsecureFileAccountStore: NMPLocalAccountCheckpoint, @unchecked Sendable {
    private let fileURL: URL
    private let lock = NSLock()

    public init(fileURL: URL) {
        self.fileURL = fileURL
    }

    func loadSecretKey() throws -> String? {
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

    func saveSecretKey(_ secretKey: String) throws {
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

    func clear() throws {
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
