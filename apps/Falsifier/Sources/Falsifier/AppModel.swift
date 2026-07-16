// M5 falsifier app -- the app's OWN state model. Nothing here is an NMP
// concept: `AppModel` is a plain `@Observable` class the app authored, and
// `NMPEngine` is just a `let` property on it, constructed exactly once at
// launch. NMP never dictates this shape -- there is no base class, no
// required `@Environment` container, no engine-owned app lifecycle.

import Foundation
import Observation
import NMP

/// The app's own account record. NMP holds the KEY (once added via
/// `addAccount`); this app tracks only the label + pubkey it wants to
/// display and switch between. A "read-only" account never calls
/// `addAccount` at all -- `setActiveAccount` accepts any pubkey, keyed or
/// not (see `NMPEngine.setActiveAccount`'s doc: read-only browsing is
/// legal).
struct Account: Identifiable, Hashable {
    enum Kind: Hashable {
        case keyed
        case readOnly
    }

    let id: String
    var label: String
    var kind: Kind
}

@Observable
final class AppModel {
    /// Constructed ONCE, here, on the app's own model -- never re-created,
    /// never wrapped in a second NMP-owned container.
    let engine: NMPEngine

    /// EXACTLY two indexer relays (VISION/M5 plan §3/§0): the app supplies
    /// no other relay fact. The engine self-discovers every author's real
    /// write relays from these two alone.
    static let indexerRelays = ["wss://purplepag.es", "wss://relay.primal.net"]

    private(set) var accounts: [Account] = []
    private(set) var activePubkey: String?
    var kinds: [UInt16] = [1]
    var lastError: String?

    init() throws {
        let caches = FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask).first
        let storePath = caches?.appendingPathComponent("nmp-falsifier-store.sqlite").path
        engine = try NMPEngine(
            config: NMPConfig(storePath: storePath, indexerRelays: Self.indexerRelays)
        )
    }

    /// Add an account this process holds the secret key for (nsec/hex), then
    /// make it active. The key crosses into the engine exactly once here;
    /// this model never touches it again.
    func addKeyedAccount(secretKey: String, label: String) async {
        do {
            let registration = try await engine.addAccount(secretKey: secretKey)
            let pubkey = registration.publicKey
            upsert(Account(id: pubkey, label: label, kind: .keyed))
            setActive(pubkey)
        } catch {
            lastError = "\(error)"
        }
    }

    /// Add a read-only account by pubkey alone -- no secret key involved.
    /// Legal per `setActiveAccount`'s contract; used here for the
    /// well-known read-only demo target.
    func addReadOnlyAccount(pubkeyHex: String, label: String) {
        upsert(Account(id: pubkeyHex, label: label, kind: .readOnly))
        setActive(pubkeyHex)
    }

    func setActive(_ pubkey: String?) {
        do {
            try engine.setActiveAccount(pubkey)
            activePubkey = pubkey
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
}
