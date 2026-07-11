// `NMPEngine` -- the ONE construction call the M4 kill test requires
// (plan §7): everything past `init` is a method call on this object, never
// a second container/provider the app must adopt.

import NMPFFI

/// Construction config for `NMPEngine`. The only relay facts this app ever
/// supplies are the three operator-configured lanes -- `indexerRelays`,
/// `appRelays`, `fallbackRelays` (`routing-and-ownership.md` §2.1): the
/// engine self-navigates outbox routing from there on its own (M5's
/// self-bootstrapping outbox -- it discovers each author's NIP-65 write
/// relays live via its own internal kind:10002 reads against
/// `indexerRelays`, and re-routes content atoms to them as they resolve).
/// No pre-resolved write-relay map is needed or accepted -- see `nmp-ffi`'s
/// own `NmpEngineConfig` doc.
public struct NMPConfig: Sendable {
    /// `nil` -> in-memory store (nothing survives a restart). A path ->
    /// a persistent store reopened at that path across launches.
    public var storePath: String?
    public var indexerRelays: [String]
    /// Operator app relay set (`Lane::AppRelay`) -- every kind, every
    /// author, always, additive. Default empty.
    public var appRelays: [String]
    /// Operator fallback relay set (`Lane::Fallback`) -- tops up authors
    /// under the 2-relay-min, suppressed when `appRelays` is non-empty.
    /// Default empty.
    public var fallbackRelays: [String]

    public init(
        storePath: String? = nil,
        indexerRelays: [String] = [],
        appRelays: [String] = [],
        fallbackRelays: [String] = []
    ) {
        self.storePath = storePath
        self.indexerRelays = indexerRelays
        self.appRelays = appRelays
        self.fallbackRelays = fallbackRelays
    }

    func toFfi() -> NmpEngineConfig {
        NmpEngineConfig(
            storePath: storePath,
            indexerRelays: indexerRelays,
            appRelays: appRelays,
            fallbackRelays: fallbackRelays
        )
    }
}

/// The engine object a dev constructs exactly once. Holds zero app-lifecycle
/// concepts -- no scene-phase hook, no required provider/environment
/// wrapper. `import NMP; let nmp = try NMPEngine(config: .init(...))` is the
/// entire adoption cost.
public final class NMPEngine: Sendable {
    let ffi: NmpEngineProtocol

    public init(config: NMPConfig) throws {
        ffi = try nmpRethrowing { try NmpEngine(config: config.toFfi()) }
    }

    /// Only for tests / fakes: wrap an already-constructed FFI object.
    init(ffi: NmpEngineProtocol) {
        self.ffi = ffi
    }

    // MARK: - Identity (P3; multi-account)

    /// Register an account from its secret key (hex or bech32 `nsec`). The
    /// key crosses this boundary exactly once and lives engine-side from
    /// this point on. Returns the account's hex public key. Does NOT make
    /// the account active -- call `setActiveAccount` for that.
    public func addAccount(secretKey: String) async throws -> String {
        try nmpRethrowing { try ffi.addAccount(secretKey: secretKey) }
    }

    /// Re-root every reactive query AND the active signing capability
    /// together onto `pubkey` (`nil` -> logged-out / read-only browsing).
    public func setActiveAccount(_ pubkey: String?) throws {
        try nmpRethrowing { try ffi.setActiveAccount(pubkey: pubkey) }
    }

    // MARK: - Read noun

    /// Open a live, detachable query. The returned `NMPQuery` is the
    /// primary read handle -- iterate it directly with `for await`; demand
    /// is dropped automatically when the query (or its iterator) is
    /// released (see `NMPQuery`'s own doc).
    public func observe(_ filter: NMPFilter) throws -> NMPQuery {
        try NMPQuery(engine: ffi, filter: filter.toFfi())
    }

    // MARK: - Diagnostics (M5) -- "the acceptance test rendered on screen,
    // permanently": per-relay wire-sub count, the exact wire filters sent,
    // events actually received per relay per kind, and per-filter coverage.
    // Read-only, off the data path -- never influences routing/delivery.

    /// Open a live diagnostics stream. The returned `NMPDiagnostics` is
    /// iterated the same way as `NMPQuery` -- teardown is deinit-tied. Throws
    /// `NMPError.engineClosed` if called after `shutdown()`.
    public func observeDiagnostics() throws -> NMPDiagnostics {
        try NMPDiagnostics(engine: ffi)
    }

    // MARK: - Lifecycle

    /// Stop the engine. Idempotent. Also called automatically on `deinit` as
    /// a safety net -- an app that forgets to call this explicitly does not
    /// leak the engine thread.
    public func shutdown() {
        ffi.shutdown()
    }

    deinit {
        ffi.shutdown()
    }
}
