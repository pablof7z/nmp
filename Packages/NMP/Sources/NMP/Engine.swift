// `NMPEngine` -- the ONE construction call the M4 kill test requires
// (plan §7): everything past `init` is a method call on this object, never
// a second container/provider the app must adopt.

import Foundation
import NMPFFI

// nmp-native:if nip65
/// Runtime inputs for outbox routing selected by this app. NMP never supplies
/// hidden indexers.
public struct OutboxRoutingConfig: Sendable {
    public var indexers: [String]

    public init(indexers: [String]) {
        self.indexers = indexers
    }
}
// nmp-native:endif

/// Construction config for `NMPEngine`. Build-time feature selection controls
/// which fields exist; runtime relay values remain app-owned inputs.
public struct NMPConfig: Sendable {
    /// `nil` -> in-memory store (nothing survives a restart). A path ->
    /// a persistent store reopened at that path across launches.
    public var storePath: String?
    /// Operator app relay set (`Lane::OperatorApp`) -- every kind, every
    /// author, always, additive. Default empty.
    public var appRelays: [String]
    /// Operator fallback relay set (`Lane::OperatorFallback`) -- tops up authors
    /// under the 2-relay-min, suppressed when `appRelays` is non-empty.
    /// Default empty.
    public var fallbackRelays: [String]
    // nmp-native:if nip65
    /// Optional outbox routing. `nil` constructs an explicit-routing-only
    /// engine. A configured capability must name at least one app-owned
    /// indexer or construction throws.
    public var outboxRouting: OutboxRoutingConfig?
    // nmp-native:endif
    /// Local/private relay HOSTS to re-admit from OTHER PEOPLE's data. A
    /// loopback / RFC-1918 / link-local relay named by someone else's relay
    /// list or event is refused by default; listing its host here
    /// (`"127.0.0.1"`, `"localhost"`) re-admits that exact host from any
    /// source. Host-only match (port- and path-insensitive).
    ///
    /// NOT how an app reaches its own local relay: relays this app declared
    /// (`appRelays`, `fallbackRelays`, an explicit write route, a pinned read
    /// scope) and relays a signed-in identity declared in its own relay list
    /// are heeded on their provenance alone. Default empty.
    public var allowedLocalRelayHosts: [String]
    /// Whether this process can reach a Tor hidden service.
    ///
    /// `.onion` is a reachability question, not a "my network" address, so
    /// `allowedLocalRelayHosts` grants it nothing. Declaring reachability
    /// makes OTHER people's `.onion` relays usable, not only ones this app or
    /// its own identities declared. NMP installs no Tor transport and never
    /// probes for one: this states that reachability exists, and a hidden
    /// service that turns out unreachable simply fails to connect.
    /// Default `false`.
    public var torReachable: Bool
    /// The one whole-engine relay ceiling. It bounds the complete compiled
    /// demand and simultaneous physical transport workers with the same
    /// effective value. Access contexts never share a socket; competing read
    /// and write contexts for the same admitted relay time-share its slot and
    /// the read is restored afterward, so apps do not multiply this value per
    /// context. Legacy zero is normalized to the finite default, never
    /// uncapped.
    public var maxRelays: UInt32
    /// Shared ceiling for live local-account signer and AUTH-policy
    /// registrations. Zero deliberately admits none.
    public var maxAuthCapabilities: UInt32

    public init(
        storePath: String? = nil,
        appRelays: [String] = [],
        fallbackRelays: [String] = [],
        // nmp-native:if nip65
        outboxRouting: OutboxRoutingConfig? = nil,
        // nmp-native:endif
        allowedLocalRelayHosts: [String] = [],
        torReachable: Bool = false,
        maxRelays: UInt32 = 10,
        maxAuthCapabilities: UInt32 = 64
    ) {
        self.storePath = storePath
        self.appRelays = appRelays
        self.fallbackRelays = fallbackRelays
        // nmp-native:if nip65
        self.outboxRouting = outboxRouting
        // nmp-native:endif
        self.allowedLocalRelayHosts = allowedLocalRelayHosts
        self.torReachable = torReachable
        self.maxRelays = maxRelays
        self.maxAuthCapabilities = maxAuthCapabilities
    }

    func toFfi() -> NmpEngineConfig {
        NmpEngineConfig(
            storePath: storePath,
            appRelays: appRelays,
            fallbackRelays: fallbackRelays,
            // nmp-native:if nip65
            outboxRouting: outboxRouting.map { FfiOutboxRoutingConfig(indexers: $0.indexers) },
            // nmp-native:endif
            allowedLocalRelayHosts: allowedLocalRelayHosts,
            torReachable: torReachable,
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
    let ffi: NmpEngineProtocol
    public let session: NMPSession

    /// Destructively remove one unowned persistent NMP store. A live engine in
    /// this OR ANY OTHER process using the same canonical path throws
    /// `NMPError.storeStillOpen` without touching the file; call `shutdown()`
    /// or release that engine first. The refusal is a cross-process exclusive
    /// ownership lock (#489), not a process-local guard: constructing a second
    /// `NMPEngine` over a live store path throws `NMPError.storeAlreadyOpen`.
    public static func resetPersistentStore(at storePath: String) throws {
        try nmpRethrowing {
            try NMPFFI.resetPersistentStore(storePath: storePath)
        }
    }

    public init(config: NMPConfig, sessionPayload: NMPSessionPayload? = nil) throws {
        let ffi = try nmpRethrowing {
            try NmpEngine(config: config.toFfi(), sessionPayload: sessionPayload?.ffi)
        }
        self.ffi = ffi
        session = NMPSession(ffi: ffi)
    }

    /// Only for tests / fakes: wrap an already-constructed FFI object.
    init(ffi: NmpEngineProtocol) {
        self.ffi = ffi
        session = NMPSession(ffi: ffi)
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
    /// `.agnostic` `NMPCacheMode`. One demand is one branch, so this is
    /// exactly `observe(.single(demand))`.
    public func observe(_ demand: NMPDemand, window: Window? = nil) throws -> NMPQuery {
        try observe(.single(demand), window: window)
    }

    /// Open a live, detachable query over several independent `NMPDemand`
    /// branches (#1108). The branches are observed through ONE handle: rows
    /// are unioned by event id with provenance merged, every batch carries
    /// one evidence entry per canonical branch, and one teardown withdraws
    /// every branch exactly once.
    ///
    /// Throws `NMPError.emptyQueryUnion`, `.aggregateResultLimitZero`,
    /// `.nestedAggregateResultLimit` or `.tooManyQueryBranches` for a
    /// declaration that can never be observed, and
    /// `.windowAggregateResultLimit` when a window and an aggregate result
    /// limit would both own the merged row count.
    public func observe(_ query: NMPLiveQuery, window: Window? = nil) throws -> NMPQuery {
        try NMPQuery(engine: ffi, liveQuery: query.toFfi(), window: window?.toFfi())
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
