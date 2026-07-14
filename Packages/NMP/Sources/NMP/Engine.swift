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
    /// Finite zero-queue native observer/action/waiter ceiling.
    public var maxNativeTasks: UInt32

    public init(
        storePath: String? = nil,
        indexerRelays: [String] = [],
        appRelays: [String] = [],
        fallbackRelays: [String] = [],
        maxNativeTasks: UInt32 = 12
    ) {
        self.storePath = storePath
        self.indexerRelays = indexerRelays
        self.appRelays = appRelays
        self.fallbackRelays = fallbackRelays
        self.maxNativeTasks = maxNativeTasks
    }

    func toFfi() -> NmpEngineConfig {
        NmpEngineConfig(
            storePath: storePath,
            indexerRelays: indexerRelays,
            appRelays: appRelays,
            fallbackRelays: fallbackRelays,
            maxNativeTasks: maxNativeTasks
        )
    }
}

/// The engine object a dev constructs exactly once. Holds zero app-lifecycle
/// concepts -- no scene-phase hook, no required provider/environment
/// wrapper. `import NMP; let nmp = try NMPEngine(config: .init(...))` is the
/// entire adoption cost.
public final class NMPEngine: Sendable {
    let ffi: NmpEngineProtocol
    private let localAccountStore: NMPInsecureFileAccountStore?

    /// Destructively remove one closed persistent NMP store. The configured
    /// local-account checkpoint is a separate file and is not touched.
    public static func resetPersistentStore(at storePath: String) throws {
        try nmpRethrowing {
            try NMPFFI.resetPersistentStore(storePath: storePath)
        }
    }

    /// Construct an engine and, when explicitly configured, restore the local
    /// account held by NMP's plaintext app-sandbox file provider.
    public init(
        config: NMPConfig,
        localAccountStore: NMPInsecureFileAccountStore? = nil
    ) throws {
        let ffi = try nmpRethrowing { try NmpEngine(config: config.toFfi()) }
        self.ffi = ffi
        self.localAccountStore = localAccountStore
        do {
            if let secretKey = try localAccountStore?.loadSecretKey() {
                let pubkey = try nmpRethrowing {
                    try ffi.addAccount(secretKey: secretKey)
                }
                try nmpRethrowing { try ffi.setActiveAccount(pubkey: pubkey) }
            }
        } catch {
            ffi.shutdown()
            throw error
        }
    }

    /// Only for tests / fakes: wrap an already-constructed FFI object.
    init(ffi: NmpEngineProtocol) {
        self.ffi = ffi
        localAccountStore = nil
    }

    // MARK: - Identity (P3; multi-account)

    /// Register an account from its secret key (hex or bech32 `nsec`). The
    /// key crosses this boundary exactly once and lives engine-side from
    /// this point on. When an `NMPInsecureFileAccountStore` was explicitly
    /// configured, NMP also checkpoints it for restart restoration. Returns
    /// the account's hex public key. Does NOT make the account active -- call
    /// `setActiveAccount` for that.
    public func addAccount(secretKey: String) async throws -> String {
        let pubkey = try nmpRethrowing {
            try ffi.addAccount(secretKey: secretKey)
        }
        try localAccountStore?.saveSecretKey(secretKey)
        return pubkey
    }

    /// Re-root every reactive query AND the active signing capability
    /// together onto `pubkey` (`nil` -> logged-out / read-only browsing).
    public func setActiveAccount(_ pubkey: String?) throws {
        try nmpRethrowing { try ffi.setActiveAccount(pubkey: pubkey) }
    }

    /// The Rust-owned account currently rooting reactive identity and writes.
    /// No secret or signer capability crosses this boundary.
    public func activeAccount() throws -> String? {
        try nmpRethrowing { try ffi.activeAccount() }
    }

    func nativeTaskCensus() -> FfiNativeTaskCensus {
        ffi.nativeTaskCensus()
    }

    func awaitNativeTasksIdle() {
        ffi.awaitNativeTasksIdle()
    }

    /// Remove the configured plaintext checkpoint. The live signer remains in
    /// this engine until the caller shuts the engine down.
    public func clearPersistedAccount() throws {
        try localAccountStore?.clear()
    }

    // MARK: - Read noun

    /// Open a live, detachable query. The returned `NMPQuery` is the
    /// primary read handle -- iterate it directly with `for await`; demand
    /// is dropped automatically when the query (or its iterator) is
    /// released (see `NMPQuery`'s own doc).
    public func observe(_ filter: NMPFilter) throws -> NMPQuery {
        try NMPQuery(engine: ffi, filter: filter.toFfi())
    }

    /// Open a live, detachable query over an explicit `NMPDemand` (#107) --
    /// the constructor to reach for once `observe(_ filter:)`'s implicit
    /// `AuthorOutboxes`/`Public` default isn't enough: declaring `.pinned`
    /// wire authority, a non-default `NMPAccessContext`, or a non-
    /// `.agnostic` `NMPCacheMode`. Same deinit-tied teardown as the
    /// `NMPFilter` overload.
    public func observe(_ demand: NMPDemand) throws -> NMPQuery {
        try NMPQuery(engine: ffi, demand: demand.toFfi())
    }

    /// Open one coordinated bounded-history specialization of the read noun.
    /// The demand owns selection/source/access/cache identity; `pageSize`
    /// bounds each advance and `maxRows` bounds every engine/FFI/Swift state.
    public func observeHistory(
        _ demand: NMPDemand,
        pageSize: UInt64,
        maxRows: UInt64
    ) throws -> NMPHistoryQuery {
        try NMPHistoryQuery(
            engine: ffi,
            demand: demand.toFfi(),
            pageSize: pageSize,
            maxRows: maxRows
        )
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

    // MARK: - Relay information (NIP-11)

    /// Acquire the relay's NIP-11 representation once. `.useCache` returns
    /// a still-fresh shared value immediately; `.refresh` revalidates it.
    /// Concurrent callers share one engine-owned HTTP flight.
    public func relayInformation(
        for relay: String,
        policy: RelayInformationCachePolicy = .useCache
    ) async throws -> RelayInformation {
        let value = try await nmpRethrowingAsync {
            try await ffi.relayInformation(relay: relay, policy: policy.toFfi())
        }
        return RelayInformation(value)
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
