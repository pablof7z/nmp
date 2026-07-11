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
    public var id: String { relay }

    public let relay: String
    public let wireSubCount: UInt32
    public let authorsServed: UInt32
    public let byLane: [LaneCount]
    /// The EXACT wire JSON of every filter currently sent to this relay.
    public let filters: [String]
    public let eventsByKind: [KindCount]
    public let coverage: [FilterCoverage]

    init(_ ffi: FfiRelayDiagnostics) {
        relay = ffi.relay
        wireSubCount = ffi.wireSubCount
        authorsServed = ffi.authorsServed
        byLane = ffi.byLane.map(LaneCount.init)
        filters = ffi.filters
        eventsByKind = ffi.eventsByKind.map(KindCount.init)
        coverage = ffi.coverage.map(FilterCoverage.init)
    }
}

/// The engine-global diagnostics snapshot (M5 plan §1.1) -- one snapshot
/// covers every currently-planned relay. Delivered by `NMPDiagnostics`
/// (`observeDiagnostics()`), pushed reactively, never polled.
public struct DiagnosticsSnapshot: Sendable {
    public let relays: [RelayDiagnostics]
    public let uncoveredAuthorCount: UInt32
    public let droppedMergeRules: [String]

    init(_ ffi: FfiDiagnosticsSnapshot) {
        relays = ffi.relays.map(RelayDiagnostics.init)
        uncoveredAuthorCount = ffi.uncoveredAuthorCount
        droppedMergeRules = ffi.droppedMergeRules
    }

    /// A default empty snapshot -- used as the initial value of
    /// `NMPDiagnosticsSnapshotObserver.snapshot` before the first real
    /// snapshot arrives.
    public init(
        relays: [RelayDiagnostics] = [],
        uncoveredAuthorCount: UInt32 = 0,
        droppedMergeRules: [String] = []
    ) {
        self.relays = relays
        self.uncoveredAuthorCount = uncoveredAuthorCount
        self.droppedMergeRules = droppedMergeRules
    }
}
