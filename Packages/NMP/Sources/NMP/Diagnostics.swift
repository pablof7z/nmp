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

/// One (lane, count) pair -- how many of a relay's wire subscriptions trace
/// to each neutral routing source (author outbound, hint, operator app, ...).
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
    // One relay URL can host distinct sessions (#8: unauthenticated versus
    // bound to an identity), so identity must include that key or two rows
    // on the same URL would collide.
    public var id: String {
        guard let authenticateAs else { return relay }
        return "\(relay)#nip42:\(authenticateAs)"
    }

    public let relay: String
    /// The identity this session is bound to, hex; `nil` if bound to none.
    /// The frozen identity of the physical session these diagnostics describe
    /// (#8): the same relay unauthenticated versus bound to a key is a
    /// distinct session with its own row.
    public let authenticateAs: String?
    public let wireSubCount: UInt32
    /// This relay's own advertised concurrent-subscription budget (NIP-11
    /// `limitation.max_subscriptions`, #931). `nil` means the relay
    /// advertised nothing and is therefore UNBUDGETED -- never a fabricated
    /// default.
    public let subscriptionBudget: UInt32?
    /// Subscriptions this relay's advertised budget removed from the plan.
    /// Non-zero means real demand did not reach the wire, and the affected
    /// queries say so through their own acquisition evidence.
    public let subscriptionsRefused: UInt32
    /// This relay's advertised `limitation.max_subid_length`.
    public let subidLengthLimit: UInt32?
    /// True iff that advertised length is shorter than the 64-character
    /// subscription ids NMP sends, i.e. this relay rejects every REQ.
    public let subidLengthRejectsOurIds: Bool
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
        authenticateAs = ffi.authenticateAs
        wireSubCount = ffi.wireSubCount
        subscriptionBudget = ffi.subscriptionBudget
        subscriptionsRefused = ffi.subscriptionsRefused
        subidLengthLimit = ffi.subidLengthLimit
        subidLengthRejectsOurIds = ffi.subidLengthRejectsOurIds
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
///
/// `phase` is the sole owner of the AUTH lifecycle: "transport accepted the
/// AUTH event" is `.awaitingRelayAck` or `.ready`, and "the relay's OK was
/// correlated" is `.ready`. No boolean restates either -- a second owner is
/// a second thing that can disagree with the first.
public struct AuthDiagnostics: Sendable, Hashable {
    public let relay: String
    /// The identity this session is bound to, hex; `nil` if bound to none.
    public let authenticateAs: String?
    public let transportGeneration: UInt64
    public let epochSequence: UInt64?
    public let challengeDescriptor: String?
    public let phase: AuthPhase
    public let policyBound: Bool
    public let signerBound: Bool
    public let authEventID: String?

    init(_ ffi: FfiAuthDiagnostics) {
        relay = ffi.relay
        authenticateAs = ffi.authenticateAs
        transportGeneration = ffi.transportGeneration
        epochSequence = ffi.epochSequence
        challengeDescriptor = ffi.challengeDescriptor
        phase = AuthPhase(ffi.phase)
        policyBound = ffi.policyBound
        signerBound = ffi.signerBound
        authEventID = ffi.authEventId
    }
}

/// Where a durable write obligation is stuck. Three stages, kept apart
/// because an app acts on each differently and because one rolled-up
/// "stuck" tells nobody anything.
public enum StalledWriteStage: Sendable, Hashable {
    /// No destination could be computed.
    case unroutable
    /// No signer answers for the author this write was FROZEN to -- never
    /// the mutable current account.
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
    /// What this write is waiting for. For `.unroutable` it is the
    /// receipt's OWN park reason, verbatim, so an operator holding both
    /// never has to decide whether two differently-worded sentences are the
    /// same fact. Never empty.
    public let detail: String
    /// When the obligation was ACCEPTED (Unix seconds), replayed verbatim
    /// across restarts. The age is `now - stalledSince`; NMP reports the
    /// instant rather than a duration because a duration baked into a
    /// snapshot goes stale exactly while nothing is happening.
    ///
    /// Known imprecision: this is when the OBLIGATION was accepted, not when
    /// the stall began. The two coincide for `.unroutable` and
    /// `.unsignable`; for `.undeliverable` it is EARLIER, so subtracting
    /// over-reports how long delivery has been failing. The park instant has
    /// no durable home yet, and an in-memory one would reset on restart.
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
    /// Relay session candidates refused by the single whole-demand ceiling,
    /// plus any defense-in-depth dial refusal at the transport boundary.
    public let sessionsRejectedOverCap: UInt64
    /// Relay sessions refused outright because the relay advertised ZERO
    /// concurrent subscriptions. Kept apart from `sessionsRejectedOverCap`:
    /// one says the plan was too wide for the operator's ceiling, the other
    /// says this relay will hold nothing open.
    public let sessionsRefusedBySubscriptionBudget: UInt64
    /// Non-`nil` once an ingest/read store door has degraded the local
    /// cache to read-only (issue #122). Observer-visible only -- never a
    /// routing input.
    public let storeDegraded: String?
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
        sessionsRejectedOverCap = ffi.sessionsRejectedOverCap
        sessionsRefusedBySubscriptionBudget = ffi.sessionsRefusedBySubscriptionBudget
        storeDegraded = ffi.storeDegraded
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
        sessionsRejectedOverCap: UInt64 = 0,
        sessionsRefusedBySubscriptionBudget: UInt64 = 0,
        storeDegraded: String? = nil,
        transportDegraded: String? = nil,
        stalledWrites: [StalledWrite] = [],
        stalledWriteTotals: StalledWriteTotals = StalledWriteTotals()
    ) {
        self.relays = relays
        self.authSessions = authSessions
        self.uncoveredAuthorCount = uncoveredAuthorCount
        self.droppedMergeRules = droppedMergeRules
        self.sessionsRejectedOverCap = sessionsRejectedOverCap
        self.sessionsRefusedBySubscriptionBudget = sessionsRefusedBySubscriptionBudget
        self.storeDegraded = storeDegraded
        self.transportDegraded = transportDegraded
        self.stalledWrites = stalledWrites
        self.stalledWriteTotals = stalledWriteTotals
    }
}
