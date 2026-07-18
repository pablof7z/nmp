// `NMPEngine` -- the ONE construction call the M4 kill test requires
// (plan §7): everything past `init` is a method call on this object, never
// a second container/provider the app must adopt.

import Foundation
import NMPFFI

/// A checkpoint write failed and the required exact live-account rollback
/// could not be proven. `persistenceError` remains the primary failure;
/// `rollbackFailure` records why a live signer may still be installed.
public struct NMPAccountCheckpointRollbackError: Error {
    public enum RollbackFailure {
        case registrationWasNotActive
        case removalFailed(any Error)
    }

    public let persistenceError: any Error
    public let rollbackFailure: RollbackFailure
}

/// The engine-side account removal succeeded but the configured plaintext
/// checkpoint could not be cleared, so the removed account could still
/// resurrect on the next launch. The removal stands -- the registration is
/// spent and `removeAccount` would now return `false` -- and the caller
/// retries the file cleanup with `clearPersistedAccount()`.
public struct NMPAccountCheckpointClearError: Error {
    /// Why the checkpoint file could not be removed.
    public let underlying: any Error
}

func rethrowCheckpointFailureAfterRollback(
    _ persistenceError: any Error,
    rollback: () throws -> Bool
) throws -> Never {
    do {
        guard try rollback() else {
            throw NMPAccountCheckpointRollbackError(
                persistenceError: persistenceError,
                rollbackFailure: .registrationWasNotActive
            )
        }
    } catch let composite as NMPAccountCheckpointRollbackError {
        throw composite
    } catch {
        throw NMPAccountCheckpointRollbackError(
            persistenceError: persistenceError,
            rollbackFailure: .removalFailed(error)
        )
    }
    throw persistenceError
}

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
    /// Local/private relay HOSTS the operator explicitly opts into despite
    /// the SSRF admission policy (issue #121). A DISCOVERED (network-sourced
    /// kind:10002) relay on a loopback / RFC-1918 / link-local / `.onion`
    /// host is rejected by default; listing its host here (e.g. `"127.0.0.1"`
    /// or `"localhost"`) re-admits discovered relays on that exact host.
    /// Host-only match (port- and path-insensitive). Default empty.
    public var allowedLocalRelayHosts: [String]
    /// The one whole-engine relay ceiling. It bounds the complete compiled
    /// demand and the transport worker set with the same effective value.
    /// Legacy zero is normalized to the finite default, never uncapped.
    public var maxRelays: UInt32
    /// Shared ceiling for live local-account signer and AUTH-policy
    /// registrations. Zero deliberately admits none.
    public var maxAuthCapabilities: UInt32

    public init(
        storePath: String? = nil,
        indexerRelays: [String] = [],
        appRelays: [String] = [],
        fallbackRelays: [String] = [],
        allowedLocalRelayHosts: [String] = [],
        maxRelays: UInt32 = 10,
        maxAuthCapabilities: UInt32 = 64
    ) {
        self.storePath = storePath
        self.indexerRelays = indexerRelays
        self.appRelays = appRelays
        self.fallbackRelays = fallbackRelays
        self.allowedLocalRelayHosts = allowedLocalRelayHosts
        self.maxRelays = maxRelays
        self.maxAuthCapabilities = maxAuthCapabilities
    }

    func toFfi() -> NmpEngineConfig {
        NmpEngineConfig(
            storePath: storePath,
            indexerRelays: indexerRelays,
            appRelays: appRelays,
            fallbackRelays: fallbackRelays,
            allowedLocalRelayHosts: allowedLocalRelayHosts,
            maxRelays: maxRelays,
            maxAuthCapabilities: maxAuthCapabilities
        )
    }
}

/// The engine object a dev constructs exactly once. Holds zero app-lifecycle
/// concepts -- no scene-phase hook, no required provider/environment
/// wrapper. `import NMP; let nmp = try NMPEngine(config: .init(...))` is the
/// entire adoption cost.
public final class NMPEngine: Sendable {
    /// Lock-guarded record of which pubkey the configured checkpoint file
    /// currently holds (#529), plus -- when that pubkey was installed on the
    /// init-restore path -- the exact `NMPAccountRegistration` created for it
    /// (#589). Set on the init restore path (`setRestored`) and on every
    /// successful `addAccount` checkpoint save or `removeAccount`/
    /// `clearPersistedAccount` clear (plain `set`, which always drops the
    /// restored registration: a later `addAccount` or a clear both mean the
    /// checkpoint no longer holds -- or no longer only holds -- the
    /// originally-restored installation). Consulted by `removeAccount` so
    /// removing that exact account also clears the on-disk checkpoint
    /// instead of letting it resurrect on restart, and by
    /// `detachPersistedAccount` so it can recover the exact restored
    /// registration to detach.
    private final class CheckpointTracker: @unchecked Sendable {
        private let lock = NSLock()
        private var pubkey: String?
        private var restoredRegistration: NMPAccountRegistration?

        func set(_ value: String?) {
            lock.withLock {
                pubkey = value
                restoredRegistration = nil
            }
        }

        func holds(_ candidate: String) -> Bool {
            lock.withLock { pubkey == candidate }
        }

        /// Record the exact registration created on the init-restore path.
        func setRestored(_ registration: NMPAccountRegistration) {
            lock.withLock {
                pubkey = registration.publicKey
                restoredRegistration = registration
            }
        }

        /// The restored registration, but only while the checkpoint still
        /// holds exactly that pubkey -- `nil` when there was no restored
        /// registration, a previous detach/removal already spent it, or a
        /// later `addAccount` has since overwritten the checkpoint.
        func restoredRegistrationIfCurrent() -> NMPAccountRegistration? {
            lock.withLock {
                guard let restoredRegistration, pubkey == restoredRegistration.publicKey else {
                    return nil
                }
                return restoredRegistration
            }
        }
    }

    let ffi: NmpEngineProtocol
    private let localAccountStore: (any NMPLocalAccountCheckpoint)?
    private let checkpointedPubkey = CheckpointTracker()

    /// Destructively remove one closed persistent NMP store. A live engine in
    /// this process using the same canonical path throws
    /// `NMPError.storeStillOpen` without touching the file; call `shutdown()`
    /// or release that engine first. This guard is process-local. The
    /// configured local-account checkpoint is a separate file and is not
    /// touched.
    public static func resetPersistentStore(at storePath: String) throws {
        try nmpRethrowing {
            try NMPFFI.resetPersistentStore(storePath: storePath)
        }
    }

    /// Construct an engine and, when a checkpoint store is explicitly
    /// configured, restore the account it holds: ANY conforming
    /// `NMPLocalAccountCheckpoint` -- a platform-vault provider, an
    /// app-custom store, or `NMPInsecureFileAccountStore` -- is a drop-in;
    /// its `loadSecretKey` drives the restore and the restored account
    /// becomes active before init returns.
    public convenience init(
        config: NMPConfig,
        localAccountStore: (any NMPLocalAccountCheckpoint)? = nil
    ) throws {
        try self.init(config: config, localAccountCheckpoint: localAccountStore)
    }

    /// Internal injection seam for deterministic checkpoint-failure tests.
    init(
        config: NMPConfig,
        localAccountCheckpoint: (any NMPLocalAccountCheckpoint)?
    ) throws {
        let ffi = try nmpRethrowing { try NmpEngine(config: config.toFfi()) }
        self.ffi = ffi
        self.localAccountStore = localAccountCheckpoint
        do {
            if let secretKey = try localAccountCheckpoint?.loadSecretKey() {
                let ffiRegistration = try nmpRethrowing {
                    try ffi.addAccount(secretKey: secretKey)
                }
                let registration = NMPAccountRegistration(ffi: ffiRegistration)
                try nmpRethrowing { try ffi.setActiveAccount(pubkey: registration.publicKey) }
                checkpointedPubkey.setRestored(registration)
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

    /// Generate and register a brand-new local account (#588) -- the
    /// NMP-owned door for a clean-start client that has no existing secret
    /// material to hand in. Composes one keygen-only FFI call with the
    /// existing `addAccount(secretKey:)`, so it inherits that method's
    /// save-with-rollback choreography and checkpoint tracking wholesale
    /// rather than a second, parallel registration pipeline. Mirrors
    /// `addAccount`'s own "does not activate" semantics -- use
    /// `registration.publicKey` with `setActiveAccount` for that.
    public func generateAccount() async throws -> NMPAccountRegistration {
        let secretKey = NMPFFI.generateAccountSecretKey()
        return try await addAccount(secretKey: secretKey)
    }

    /// Register an account from its secret key (hex or bech32 `nsec`). The
    /// key crosses this boundary exactly once and lives engine-side from
    /// this point on. When an `NMPLocalAccountCheckpoint` was explicitly
    /// configured, NMP also checkpoints it for restart restoration. Returns
    /// the opaque exact registration required for stale-safe removal. Does
    /// NOT make the account active -- use `registration.publicKey` with
    /// `setActiveAccount` for that.
    ///
    /// If checkpoint persistence fails after the live signer is installed,
    /// this method removes that exact installation before rethrowing the
    /// original persistence error.
    public func addAccount(secretKey: String) async throws -> NMPAccountRegistration {
        let ffiRegistration = try nmpRethrowing {
            try ffi.addAccount(secretKey: secretKey)
        }
        let registration = NMPAccountRegistration(ffi: ffiRegistration)
        if let localAccountStore {
            do {
                try localAccountStore.saveSecretKey(secretKey)
            } catch let persistenceError {
                try rethrowCheckpointFailureAfterRollback(persistenceError) {
                    try nmpRethrowing {
                        try ffi.removeAccount(registration: ffiRegistration)
                    }
                }
            }
            checkpointedPubkey.set(registration.publicKey)
        }
        return registration
    }

    /// Remove only the live signer installation proven by `registration`.
    /// Repeated removal and removal through a stale registration return
    /// `false`; there is deliberately no public-key removal overload.
    ///
    /// When the removal succeeds AND the configured checkpoint currently
    /// holds this exact account, the on-disk plaintext checkpoint is cleared
    /// too, so a removed account cannot resurrect on the next launch (#529).
    /// A stale removal (`false`) or a removal of an account the checkpoint
    /// does not hold never touches the checkpoint. If the engine-side
    /// removal succeeds but the checkpoint clear fails, the removal stands
    /// (the registration is spent) and this method throws
    /// `NMPAccountCheckpointClearError` -- retry the file cleanup with
    /// `clearPersistedAccount()`.
    public func removeAccount(_ registration: NMPAccountRegistration) throws -> Bool {
        let removed = try nmpRethrowing {
            try ffi.removeAccount(registration: registration.ffi)
        }
        if removed, checkpointedPubkey.holds(registration.publicKey) {
            do {
                try localAccountStore?.clear()
            } catch {
                throw NMPAccountCheckpointClearError(underlying: error)
            }
            checkpointedPubkey.set(nil)
        }
        return removed
    }

    /// Detach exactly the local account this engine restored from its
    /// configured checkpoint at construction (#589). Delegates to
    /// `removeAccount(_:)`, so it inherits that method's atomic
    /// checkpoint-clear behavior verbatim, including the typed
    /// `NMPAccountCheckpointClearError` partial-failure/recovery contract --
    /// see `removeAccount`'s doc.
    ///
    /// Returns `false` -- and removes nothing -- when there is no exact
    /// restored registration left to detach: no account was restored at
    /// construction (no checkpoint configured, or the checkpoint was empty),
    /// a previous `detachPersistedAccount()` or `removeAccount` call already
    /// spent it, a later `addAccount` has since overwritten the checkpoint
    /// with a different installation, or `clearPersistedAccount()` was
    /// called directly (it also spends the tracked restored registration --
    /// after that call the live restored signer can only be removed by
    /// shutting down the engine, never by a later `detachPersistedAccount()`
    /// call).
    public func detachPersistedAccount() throws -> Bool {
        guard let registration = checkpointedPubkey.restoredRegistrationIfCurrent() else {
            return false
        }
        return try removeAccount(registration)
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

    /// Remove the configured plaintext checkpoint. The live signer remains in
    /// this engine until the caller shuts the engine down or removes the
    /// account through its registration (which also clears the checkpoint
    /// when it holds that exact account -- see `removeAccount`). Also the
    /// documented retry path after an `NMPAccountCheckpointClearError`.
    public func clearPersistedAccount() throws {
        try localAccountStore?.clear()
        checkpointedPubkey.set(nil)
    }

    // MARK: - Read noun

    /// Open a live, detachable query. The returned `NMPQuery` is the
    /// primary read handle -- iterate it directly with `for await`; demand
    /// is dropped automatically when the query (or its iterator) is
    /// released (see `NMPQuery`'s own doc).
    ///
    /// `window` is the one bounding policy on this read noun (#485).
    /// `nil` (the default) observes the full live set through exact rebased
    /// deltas; intermediate reducer emits may conflate for a slow observer.
    /// `.expandable(initial:max:)` bounds the observation to a
    /// newest-first window delivered as authoritative snapshots, grown only
    /// by `NMPQuery.requestRows(atLeast:)` -- delivery mode is derived from
    /// that boundedness, never chosen separately (see `Window`'s doc).
    /// Throws `NMPError.windowZeroRows` / `.windowInitialExceedsMax` for an
    /// invalid window, and `.windowSelectionHasLimit` when a windowed
    /// selection already carries its own NIP-01 `limit`.
    public func observe(_ filter: NMPFilter, window: Window? = nil) throws -> NMPQuery {
        try NMPQuery(engine: ffi, filter: filter.toFfi(), window: window?.toFfi())
    }

    /// Open a live, detachable query over an explicit `NMPDemand` (#107) --
    /// the constructor to reach for once `observe(_ filter:)`'s implicit
    /// `AuthorOutboxes`/`Public` default isn't enough: declaring `.pinned`
    /// wire authority, a non-default `NMPAccessContext`, or a non-
    /// `.agnostic` `NMPCacheMode`. Same deinit-tied teardown and same
    /// optional `window` policy as the `NMPFilter` overload.
    public func observe(_ demand: NMPDemand, window: Window? = nil) throws -> NMPQuery {
        try NMPQuery(engine: ffi, demand: demand.toFfi(), window: window?.toFfi())
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
