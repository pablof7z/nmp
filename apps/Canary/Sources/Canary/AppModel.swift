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

/// One write recovered at launch from NMP's own durable publish queue,
/// paired back with a live receipt stream via `reattachReceipt(id:)`.
/// `AppModel` only recovers the handle at launch; `ComposeView` is the one
/// place that iterates `receipt.status`, so a fresh publish and a
/// reattached one are observed by the exact same code.
struct ReattachedWrite {
    let receiptID: UInt64
    let receipt: Receipt
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

    /// Writes recovered from NMP's own publish queue on THIS launch.
    /// `ComposeView` drains this once (`takeReattachedWrites()`) and starts
    /// observing each the same way it observes a freshly published write.
    private(set) var reattachedWrites: [ReattachedWrite] = []

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
        // identity is visible again, not just usable. A failure here used to
        // be swallowed with `try?`, which makes a broken session look
        // identical to an empty one -- surface it instead (#1797).
        do {
            for sessionAccount in try engine.session.accounts {
                upsert(Account(
                    id: sessionAccount.publicKey.bytes,
                    label: Self.shortLabel(for: sessionAccount.publicKey.bytes),
                    kind: sessionAccount.providerKind == nil ? .publicKeyOnly : .keyed,
                    sessionAccount: sessionAccount
                ))
            }
            if let current = try engine.session.current {
                currentPubkey = current.publicKey.bytes
            }
        } catch {
            lastError = "Session restore failed: \(error)"
        }

        // Crash-during-publication recovery (C9): NMP's own publish queue
        // already knows every open obligation durably -- enumerate it and
        // reattach to each, rather than an app-owned shadow ledger
        // (#1770). `publishQueue` is exactly the door for "what have I got
        // outstanding, and what went wrong with it"; no app-side durable
        // state is needed to find one after a restart.
        do {
            for entry in try engine.publishQueue(limit: 64) where !entry.isTerminal {
                switch try engine.reattachReceipt(id: entry.receiptID) {
                case .attached(let receipt):
                    reattachedWrites.append(ReattachedWrite(receiptID: entry.receiptID, receipt: receipt))
                case .notFound:
                    // The queue itself just reported this entry; nothing to
                    // recover if it vanished between those two calls.
                    break
                case .retainedButUnreadable:
                    // Durable evidence exists but NMP could not read it back.
                    // NOT the same as "gone" -- surface it, do not drop it
                    // silently or invent a resolved state for it.
                    lastError = "Write \(entry.receiptID) has retained but unreadable evidence."
                }
            }
        } catch {
            lastError = "\(error)"
        }
    }

    /// One-shot drain so a `ComposeView` re-appearing (e.g. tab switch) never
    /// re-observes the same reattached stream twice.
    func takeReattachedWrites() -> [ReattachedWrite] {
        defer { reattachedWrites = [] }
        return reattachedWrites
    }

    /// Generate a local-key account inside NMP and select it atomically.
    func addKeyedAccount(label: String) {
        guard let account = withSessionMutation({
            try engine.session.add(privateKey: NMPPrivateKey.generate(), makeCurrent: true)
        }) else { return }
        upsert(Account(id: account.publicKey.bytes, label: label, kind: .keyed, sessionAccount: account))
        currentPubkey = account.publicKey.bytes
    }

    /// Add the fixed public-key-only demo account and select it atomically.
    func addPublicKeyOnlyAccount(label: String) {
        guard let account = withSessionMutation({
            try engine.session.add(
                publicKey: NMPPublicKey(bytes: Data(Self.readOnlyDemoPublicKey)),
                makeCurrent: true
            )
        }) else { return }
        upsert(Account(id: account.publicKey.bytes, label: label, kind: .publicKeyOnly, sessionAccount: account))
        currentPubkey = account.publicKey.bytes
    }

    func makeCurrent(_ account: Account) {
        guard withSessionMutation({ try engine.session.makeCurrent(account.sessionAccount) }) != nil else { return }
        currentPubkey = account.id
    }

    /// Every `engine.session` mutation funnels through here so persistence
    /// follows the mutation automatically rather than being remembered per
    /// call site (#1797) -- the SDK has no change signal, so this is this
    /// app's one choke point standing in for one. Failures inside `body`
    /// (an SDK error) and failures persisting afterward (Keychain) are both
    /// surfaced through `lastError`; neither is swallowed.
    private func withSessionMutation<T>(_ body: () throws -> T) -> T? {
        do {
            let result = try body()
            persistSession()
            return result
        } catch {
            lastError = "\(error)"
            return nil
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

    /// Re-export the whole session and overwrite the stored payload. Called,
    /// via `withSessionMutation`, after every account add/select.
    ///
    /// `SecItemUpdate`-or-add, never delete-then-add: the previous
    /// implementation did `SecItemDelete` followed by `SecItemAdd`, so a
    /// crash in the gap between those two calls destroyed every account,
    /// including the local key material that makes them signable again
    /// (#1797). `Session.swift:206` calls the exported payload "suitable for
    /// atomic app storage"; delete-then-add was not using that property --
    /// update-in-place (falling back to add only when nothing exists yet)
    /// never leaves the Keychain item absent.
    ///
    /// A failed export used to be swallowed with `try?`, leaving a stale
    /// Keychain entry with no signal anywhere; both the export and the
    /// Keychain write now surface through `lastError` on failure.
    private func persistSession() {
        do {
            let payload = try engine.session.export()
            let query: [String: Any] = [
                kSecClass as String: kSecClassGenericPassword,
                kSecAttrService as String: Self.keychainService,
                kSecAttrAccount as String: Self.keychainAccount,
            ]
            let updateAttributes: [String: Any] = [
                kSecValueData as String: payload.bytes,
                kSecAttrAccessible as String: kSecAttrAccessibleAfterFirstUnlock,
            ]
            var status = SecItemUpdate(query as CFDictionary, updateAttributes as CFDictionary)
            if status == errSecItemNotFound {
                var attributes = query
                attributes[kSecValueData as String] = payload.bytes
                attributes[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlock
                status = SecItemAdd(attributes as CFDictionary, nil)
            }
            guard status == errSecSuccess else {
                lastError = "Keychain session write failed: OSStatus \(status)"
                return
            }
        } catch {
            lastError = "Session export failed: \(error)"
        }
    }
}
