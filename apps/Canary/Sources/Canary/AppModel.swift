// The Canary -- the app's OWN state model. Nothing here is an NMP concept:
// `AppModel` is a plain `@Observable` class the app authored, and
// `NMPEngine` is just a `let` property on it, constructed exactly once at
// launch. NMP never dictates this shape -- there is no base class, no
// required `@Environment` container, no engine-owned app lifecycle.

import Foundation
import Observation
import Security
import NMP

/// The app's own presentation record. NMP's session account is the operation
/// handle; this app owns only the label and display grouping around it.
struct Account: Identifiable {
    enum Kind {
        case keyed
        case publicKeyOnly
    }

    let id: Data
    var label: String
    var kind: Kind
    let sessionAccount: NMPSessionAccount
}

@Observable
final class AppModel {
    /// Constructed ONCE, here, on the app's own model -- never re-created,
    /// never wrapped in a second NMP-owned container.
    let engine: NMPEngine

    /// EXACTLY two operator-provided app relays. The native core package has
    /// no protocol provider and therefore learns no author routes implicitly.
    static let appRelays = ["wss://purplepag.es", "wss://relay.primal.net"]

    private(set) var accounts: [Account] = []
    private(set) var currentPubkey: Data?
    var kinds: [UInt16] = [1]
    var lastError: String?

    init() throws {
        let caches = FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask).first
        let storePath = caches?.appendingPathComponent("nmp-canary-store.sqlite").path
        engine = try NMPEngine(
            config: NMPConfig(storePath: storePath, appRelays: Self.appRelays),
            sessionPayload: Self.loadSessionPayload()
        )

        // The Redb store survives relaunch on its own, but accounts and the
        // current identity live only in the session NMP hands back here --
        // rebuild this app's own presentation records from it so a restored
        // identity is visible again, not just usable.
        if let restored = try? engine.session.accounts {
            for sessionAccount in restored {
                upsert(Account(
                    id: sessionAccount.publicKey.bytes,
                    label: Self.shortLabel(for: sessionAccount.publicKey.bytes),
                    kind: sessionAccount.providerKind == nil ? .publicKeyOnly : .keyed,
                    sessionAccount: sessionAccount
                ))
            }
            if let current = (try? engine.session.current) ?? nil {
                currentPubkey = current.publicKey.bytes
            }
        }
    }

    /// Generate a local-key account inside NMP and select it atomically.
    func addKeyedAccount(label: String) {
        do {
            let account = try engine.session.add(
                privateKey: NMPPrivateKey.generate(),
                makeCurrent: true
            )
            upsert(Account(
                id: account.publicKey.bytes,
                label: label,
                kind: .keyed,
                sessionAccount: account
            ))
            currentPubkey = account.publicKey.bytes
            persistSession()
        } catch {
            lastError = "\(error)"
        }
    }

    /// Add the fixed public-key-only demo account and select it atomically.
    func addPublicKeyOnlyAccount(label: String) {
        do {
            let account = try engine.session.add(
                publicKey: NMPPublicKey(bytes: Data(Self.readOnlyDemoPublicKey)),
                makeCurrent: true
            )
            upsert(Account(
                id: account.publicKey.bytes,
                label: label,
                kind: .publicKeyOnly,
                sessionAccount: account
            ))
            currentPubkey = account.publicKey.bytes
            persistSession()
        } catch {
            lastError = "\(error)"
        }
    }

    func makeCurrent(_ account: Account) {
        do {
            try engine.session.makeCurrent(account.sessionAccount)
            currentPubkey = account.id
            persistSession()
        } catch {
            lastError = "\(error)"
        }
    }

    private func upsert(_ account: Account) {
        if let idx = accounts.firstIndex(where: { $0.id == account.id }) {
            accounts[idx] = account
        } else {
            accounts.append(account)
        }
    }

    private static func shortLabel(for bytes: Data) -> String {
        let hex = bytes.map { String(format: "%02x", $0) }.joined()
        return "\(hex.prefix(8))…\(hex.suffix(8))"
    }

    private static let readOnlyDemoPublicKey: [UInt8] = [
        0x3b, 0xf0, 0xc6, 0x3f, 0xcb, 0x93, 0x46, 0x34,
        0x07, 0xaf, 0x97, 0xa5, 0xe5, 0xee, 0x64, 0xfa,
        0x88, 0x3d, 0x10, 0x7e, 0x9e, 0x55, 0x84, 0x72,
        0xc4, 0xeb, 0x9a, 0xaa, 0xef, 0xa4, 0x59, 0x0d,
    ]

    // MARK: - Session persistence
    //
    // `NMPSessionPayload` is opaque and may carry key material (it is what
    // makes a local-key account signable again after relaunch), so it goes
    // in the Keychain -- the ordinary place an iOS app puts a secret -- never
    // `UserDefaults` or a plist. `.afterFirstUnlock` keeps it usable from a
    // background relaunch (e.g. resuming a parked write) without requiring
    // the device to be unlocked at that exact moment.

    private static let keychainService = "com.nmp.canary.session"
    private static let keychainAccount = "session"

    private static func loadSessionPayload() -> NMPSessionPayload? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: keychainService,
            kSecAttrAccount as String: keychainAccount,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var result: AnyObject?
        guard SecItemCopyMatching(query as CFDictionary, &result) == errSecSuccess,
            let bytes = result as? Data
        else { return nil }
        return NMPSessionPayload(bytes: bytes)
    }

    /// Re-export the whole session and overwrite the stored payload. Called
    /// after every account add/select so a killed app resumes with the same
    /// accounts and current identity it had a moment before.
    private func persistSession() {
        guard let payload = try? engine.session.export() else { return }
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: Self.keychainService,
            kSecAttrAccount as String: Self.keychainAccount,
        ]
        SecItemDelete(query as CFDictionary)
        var attributes = query
        attributes[kSecValueData as String] = payload.bytes
        attributes[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlock
        SecItemAdd(attributes as CFDictionary, nil)
    }
}
