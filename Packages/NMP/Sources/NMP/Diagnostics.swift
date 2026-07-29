// The diagnostic surface's delivered value types, in ergonomic Swift shape
// (M5 plan §1.3) -- "the acceptance test rendered on screen, permanently."
// Mirrors `Row.swift`'s pattern exactly: no `Ffi`-prefixed type ever leaks
// past this file.

import NMPFFI

/// One (kind, count) pair -- events actually RECEIVED from a relay, counted
/// by kind.
public struct KindCount: Sendable, Hashable {
    public let kind: UInt16
    public let count: UInt64

    init(_ ffi: FfiKindCount) {
        kind = ffi.kind
        count = ffi.count
    }
}

/// One (lane, count) pair -- how many of a relay's wire subs trace to each
/// routing lane (NIP-65 write, hint, indexer discovery, ...).
public struct LaneCount: Sendable, Hashable {
    public let lane: String
    public let count: UInt32

    init(_ ffi: FfiLaneCount) {
        lane = ffi.lane
        count = ffi.count
    }
}

/// A proven, retained `[from, through]` interval -- the engine-global
/// DIAGNOSTICS watermark (`nmp_store::coverage::CoverageInterval` mirror).
/// Deliberately distinct from the scoped, per-query `AcquisitionEvidence`
/// surface (`Row.swift`) -- never reused as a query-level verdict
/// (`docs/design/scoped-evidence-49-12-plan.md` §4).
public struct CoverageInterval: Sendable, Hashable {
    public let from: UInt64
    public let through: UInt64

    init(_ ffi: FfiCoverageInterval) {
        from = ffi.from
        through = ffi.through
    }
}

/// One filter's proven coverage state at one relay. `filter` is the EXACT
/// wire JSON this coverage state is for -- the same rendering as the
/// parallel entry in `RelayDiagnostics.filters`. `coverage` is `nil` --
/// "no row = not covered", unchanged from the store's own rule.
public struct FilterCoverage: Sendable, Hashable {
    public let filter: String
    public let coverage: CoverageInterval?

    init(_ ffi: FfiFilterCoverage) {
        filter = ffi.filter
        coverage = ffi.coverage.map(CoverageInterval.init)
    }
}

/// One relay's full diagnostics: wire-sub count, lane breakdown, reverse
/// coverage (authors served), the exact filters currently sent, events
/// actually received per kind, and per-filter coverage state. Every field is
/// a REAL number read off the running engine -- never fabricated/estimated.
public struct RelayDiagnostics: Sendable, Identifiable, Hashable {
    // One relay URL can now host distinct sessions (#8: `.public` vs a
    // `.nip42` identity), so identity must include the access context or two
    // rows on the same URL would collide.
    public var id: String {
        switch access {
        case .public: return relay
        case let .nip42(publicKey): return "\(relay)#nip42:\(publicKey)"
        }
    }

    public let relay: String
    /// The frozen access identity of the physical session these diagnostics
    /// describe (#8): the same relay under `.public` versus a `.nip42`
    /// identity is a distinct session with its own row.
    public let access: NMPAccessContext
    public let wireSubCount: UInt32
    public let authorsServed: UInt32
    public let byLane: [LaneCount]
    /// The EXACT wire JSON of every filter currently sent to this relay.
    public let filters: [String]
    public let eventsByKind: [KindCount]
    public let coverage: [FilterCoverage]
    public let nip11SupportedNips: [UInt16]?
    public let nip11DocumentRevision: String?
    public let nip11Freshness: String?
    public let nip11LastError: String?
    public let nip77Advertisement: String
    public let nip77Behavior: String
    public let nip77Handoff: String

    init(_ ffi: FfiRelayDiagnostics) {
        relay = ffi.relay
        access = NMPAccessContext(ffi.access)
        wireSubCount = ffi.wireSubCount
        authorsServed = ffi.authorsServed
        byLane = ffi.byLane.map(LaneCount.init)
        filters = ffi.filters
        eventsByKind = ffi.eventsByKind.map(KindCount.init)
        coverage = ffi.coverage.map(FilterCoverage.init)
        nip11SupportedNips = ffi.nip11SupportedNips
        nip11DocumentRevision = ffi.nip11DocumentRevision
        nip11Freshness = ffi.nip11Freshness
        nip11LastError = ffi.nip11LastError
        nip77Advertisement = ffi.nip77Advertisement
        nip77Behavior = ffi.nip77Behavior
        nip77Handoff = ffi.nip77Handoff
    }
}

/// One bounded exact-session AUTH diagnostics record. The raw challenge and
/// opaque capability-instance identities remain engine-private.
public struct AuthDiagnostics: Sendable, Hashable {
    public let relay: String
    public let access: NMPAccessContext
    public let transportGeneration: UInt64
    public let epochSequence: UInt64?
    public let challengeDescriptor: String?
    public let phase: AuthPhase
    public let policyBound: Bool
    public let signerBound: Bool
    public let authEventID: String?
    public let sendHandoffAccepted: Bool
    public let relayOKAccepted: Bool

    init(_ ffi: FfiAuthDiagnostics) {
        relay = ffi.relay
        access = NMPAccessContext(ffi.access)
        transportGeneration = ffi.transportGeneration
        epochSequence = ffi.epochSequence
        challengeDescriptor = ffi.challengeDescriptor
        phase = AuthPhase(ffi.phase)
        policyBound = ffi.policyBound
        signerBound = ffi.signerBound
        authEventID = ffi.authEventId
        sendHandoffAccepted = ffi.sendHandoffAccepted
        relayOKAccepted = ffi.relayOkAccepted
    }
}

/// Where a durable write obligation is stuck. Three stages, kept apart
/// because an app acts on each differently and because one rolled-up
/// "stuck" tells nobody anything.
public enum StalledWriteStage: Sendable, Hashable {
    /// No destination could be computed.
    case unroutable
    /// No signer answers for the author this write was FROZEN to -- never
    /// the mutable active account.
    case unsignable
    /// Destinations exist and none of them is working.
    case undeliverable

    init(_ ffi: FfiStalledWriteStage) {
        switch ffi {
        case .unroutable: self = .unroutable
        case .unsignable: self = .unsignable
        case .undeliverable: self = .undeliverable
        }
    }
}

/// One durable write obligation that cannot currently progress. Read-only
/// evidence: nothing here cancels, retries, prunes or acknowledges a write.
public struct StalledWrite: Sendable, Identifiable, Hashable {
    /// A stable, restart-reproducible descriptor of this obligation.
    /// Deliberately NOT a receipt id and not parseable back into one: it
    /// exists to tell two rows apart and to recognise the same row across
    /// snapshots, never to reattach or enumerate receipts.
    public let id: String
    public let stage: StalledWriteStage
    /// What this write is waiting for, in the write plane's own recorded
    /// words. Never empty.
    public let detail: String
    /// When the obligation was ACCEPTED (Unix seconds), replayed verbatim
    /// across restarts. The age is `now - stalledSince`; NMP reports the
    /// instant rather than a duration because a duration baked into a
    /// snapshot goes stale exactly while nothing is happening.
    public let stalledSince: UInt64

    init(_ ffi: FfiStalledWrite) {
        id = ffi.id
        stage = StalledWriteStage(ffi.stage)
        detail = ffi.detail
        stalledSince = ffi.stalledSince
    }
}

/// The exact census behind `DiagnosticsSnapshot.stalledWrites`. Totals count
/// every stalled obligation, including the ones no detail row was emitted
/// for: a bound on memory is never a lie about how much is stuck.
public struct StalledWriteTotals: Sendable, Hashable {
    public let unroutable: UInt64
    public let unsignable: UInt64
    public let undeliverable: UInt64
    /// Stalled obligations with no detail row in this snapshot.
    public let omittedDetails: UInt64
    /// The detail-window bound this snapshot was built under.
    public let detailLimit: UInt64

    init(_ ffi: FfiStalledWriteTotals) {
        unroutable = ffi.unroutable
        unsignable = ffi.unsignable
        undeliverable = ffi.undeliverable
        omittedDetails = ffi.omittedDetails
        detailLimit = ffi.detailLimit
    }

    public init(
        unroutable: UInt64 = 0,
        unsignable: UInt64 = 0,
        undeliverable: UInt64 = 0,
        omittedDetails: UInt64 = 0,
        detailLimit: UInt64 = 0
    ) {
        self.unroutable = unroutable
        self.unsignable = unsignable
        self.undeliverable = undeliverable
        self.omittedDetails = omittedDetails
        self.detailLimit = detailLimit
    }
}

/// The engine-global diagnostics snapshot (M5 plan §1.1) -- one snapshot
/// covers every currently-planned relay. Delivered by `NMPDiagnostics`
/// (`observeDiagnostics()`), pushed reactively, never polled.
public struct DiagnosticsSnapshot: Sendable {
    public let relays: [RelayDiagnostics]
    public let authSessions: [AuthDiagnostics]
    public let uncoveredAuthorCount: UInt32
    public let droppedMergeRules: [String]
    public let transportDegraded: String?
    /// Every durable write obligation that cannot progress, bounded to
    /// `stalledWriteTotals.detailLimit` rows in a deterministic display
    /// order. A receipt answers "what happened to THIS write", which needs
    /// someone still holding it; this answers "is anything quietly stuck"
    /// for an app holding nothing. Reading it changes nothing.
    public let stalledWrites: [StalledWrite]
    /// Exact counts behind that window.
    public let stalledWriteTotals: StalledWriteTotals

    init(_ ffi: FfiDiagnosticsSnapshot) {
        relays = ffi.relays.map(RelayDiagnostics.init)
        authSessions = ffi.authSessions.map(AuthDiagnostics.init)
        uncoveredAuthorCount = ffi.uncoveredAuthorCount
        droppedMergeRules = ffi.droppedMergeRules
        transportDegraded = ffi.transportDegraded
        stalledWrites = ffi.stalledWrites.map(StalledWrite.init)
        stalledWriteTotals = StalledWriteTotals(ffi.stalledWriteTotals)
    }

    /// A default empty snapshot -- used as the initial value of
    /// `NMPDiagnosticsSnapshotObserver.snapshot` before the first real
    /// snapshot arrives.
    public init(
        relays: [RelayDiagnostics] = [],
        authSessions: [AuthDiagnostics] = [],
        uncoveredAuthorCount: UInt32 = 0,
        droppedMergeRules: [String] = [],
        transportDegraded: String? = nil,
        stalledWrites: [StalledWrite] = [],
        stalledWriteTotals: StalledWriteTotals = StalledWriteTotals()
    ) {
        self.relays = relays
        self.authSessions = authSessions
        self.uncoveredAuthorCount = uncoveredAuthorCount
        self.droppedMergeRules = droppedMergeRules
        self.transportDegraded = transportDegraded
        self.stalledWrites = stalledWrites
        self.stalledWriteTotals = stalledWriteTotals
    }
}
