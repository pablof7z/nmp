// M5 falsifier app -- the app's OWN state model. Nothing here is an NMP
// concept: `AppModel` is a plain `@Observable` class the app authored, and
// `NMPEngine` is just a `let` property on it, constructed exactly once at
// launch. NMP never dictates this shape -- there is no base class, no
// required `@Environment` container, no engine-owned app lifecycle.

import Foundation
import Observation
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
        let storePath = caches?.appendingPathComponent("nmp-falsifier-store.sqlite").path
        engine = try NMPEngine(
            config: NMPConfig(storePath: storePath, appRelays: Self.appRelays)
        )
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
        } catch {
            lastError = "\(error)"
        }
    }

    func makeCurrent(_ account: Account) {
        do {
            try engine.session.makeCurrent(account.sessionAccount)
            currentPubkey = account.id
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

    private static let readOnlyDemoPublicKey: [UInt8] = [
        0x3b, 0xf0, 0xc6, 0x3f, 0xcb, 0x93, 0x46, 0x34,
        0x07, 0xaf, 0x97, 0xa5, 0xe5, 0xee, 0x64, 0xfa,
        0x88, 0x3d, 0x10, 0x7e, 0x9e, 0x55, 0x84, 0x72,
        0xc4, 0xeb, 0x9a, 0xaa, 0xef, 0xa4, 0x59, 0x0d,
    ]
}
