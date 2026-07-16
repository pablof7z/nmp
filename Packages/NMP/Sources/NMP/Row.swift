// The read noun's delivered value types, in ergonomic Swift shape.
// RAW TOKENS ONLY (VISION ledger #12, inherited from `FfiRow`'s own
// contract) -- this layer adds no formatting, no display concept
// whatsoever; that stays app-owned.

import NMPFFI

/// One delivered event, verbatim. `Identifiable` so a SwiftUI `List(rows)`
/// works with zero extra ceremony (the §7 canary's whole point).
public struct Row: Sendable, Identifiable, Hashable {
    public let id: String
    public let pubkey: String
    public let createdAt: UInt64
    public let kind: UInt16
    /// Each inner array is one raw tag (`["p", "<hex>", ...]`), verbatim.
    public let tags: [[String]]
    public let content: String
    public let sig: String
    /// Sorted, deduplicated relay URLs that have delivered this event id
    /// (#105) -- raw tokens, not a formatted/display field either.
    public let sources: [String]

    init(_ ffi: FfiRow) {
        id = ffi.id
        pubkey = ffi.pubkey
        createdAt = ffi.createdAt
        kind = ffi.kind
        tags = ffi.tags
        content = ffi.content
        sig = ffi.sig
        sources = ffi.sources
    }

    /// Construct a raw row value for previews, fixtures, import adapters, and
    /// app-owned renderers. This does not insert the event into NMP or make any
    /// claim about signature validity, provenance, or canonical-store status.
    public init(
        id: String, pubkey: String, createdAt: UInt64, kind: UInt16, tags: [[String]],
        content: String, sig: String, sources: [String]
    ) {
        self.id = id
        self.pubkey = pubkey
        self.createdAt = createdAt
        self.kind = kind
        self.tags = tags
        self.content = content
        self.sig = sig
        self.sources = sources
    }

    /// A copy of this row with `sources` replaced -- the accumulator's own
    /// update when the SAME row's provenance grows (`RowDelta.sourcesGrew`,
    /// #105) with no new event to reconstruct the rest of the row from.
    func withSources(_ sources: [String]) -> Row {
        Row(
            id: id, pubkey: pubkey, createdAt: createdAt, kind: kind, tags: tags,
            content: content, sig: sig, sources: sources
        )
    }
}

/// Closed AUTH negotiation and terminal diagnostics phases
/// (populated by the #8 AUTH reducer).
public enum AuthPhase: Sendable, Hashable {
    case awaitingChallenge
    case awaitingPolicy
    case awaitingSignature
    case awaitingRelayAck
    case ready
    case denied
    case error

    init(_ ffi: FfiAuthPhase) {
        switch ffi {
        case .awaitingChallenge: self = .awaitingChallenge
        case .awaitingPolicy: self = .awaitingPolicy
        case .awaitingSignature: self = .awaitingSignature
        case .awaitingRelayAck: self = .awaitingRelayAck
        case .ready: self = .ready
        case .denied: self = .denied
        case .error: self = .error
        }
    }
}

/// The closed, honest per-source link-status vocabulary
/// (`docs/design/scoped-evidence-49-12-plan.md` §4).
public enum SourceStatus: Sendable, Hashable {
    case requesting
    case connecting
    case disconnected
    case awaitingAuth(phase: AuthPhase)
    case authDenied
    case error

    init(_ ffi: FfiSourceStatus) {
        switch ffi {
        case .requesting: self = .requesting
        case .connecting: self = .connecting
        case .disconnected: self = .disconnected
        case .awaitingAuth(let phase): self = .awaitingAuth(phase: AuthPhase(phase))
        case .authDenied: self = .authDenied
        case .error: self = .error
        }
    }
}

/// One relay's acquisition state for a query's subtree, as two deliberately
/// orthogonal facts: a durable PAST fact (`reconciledThrough`) and a current
/// LINK fact (`status`) -- a relay can be currently `.disconnected` while
/// still carrying a perfectly good `reconciledThrough` from before it
/// dropped (offline cached rows remain usable).
public struct SourceEvidence: Sendable, Hashable {
    public let relay: String
    /// The frozen access identity of the physical session that produced this
    /// per-source fact (#8): the same relay URL under `.public` versus a
    /// `.nip42` identity is a distinct, non-aliasing source.
    public let access: NMPAccessContext
    public let reconciledThrough: UInt64?
    public let status: SourceStatus

    init(_ ffi: FfiSourceEvidence) {
        relay = ffi.relay
        access = NMPAccessContext(ffi.access)
        reconciledThrough = ffi.reconciledThrough
        status = SourceStatus(ffi.status)
    }
}

/// An explicit, never-silent shortfall in a query's subtree acquisition --
/// facts about what nothing is (yet) trying to acquire, never folded into
/// `AcquisitionEvidence.sources`. `atom` is the exact wire JSON of the
/// unacquired filter shape.
public enum ShortfallFact: Sendable, Hashable {
    case noPlannedSource(atom: String)
    case noResolvedDemand
    case localLimit(atom: String)

    init(_ ffi: FfiShortfallFact) {
        switch ffi {
        case .noPlannedSource(let atom): self = .noPlannedSource(atom: atom)
        case .noResolvedDemand: self = .noResolvedDemand
        case .localLimit(let atom): self = .localLimit(atom: atom)
        }
    }
}

/// A query's scoped acquisition evidence
/// (`docs/design/scoped-evidence-49-12-plan.md` §4): per-source facts over
/// the query's full subtree, plus an explicit shortfall list. Deliberately
/// NOT a query-level verdict -- an app reads which source
/// has proven what and rolls that into its own progress policy; NMP never
/// does that rollup for it.
public struct AcquisitionEvidence: Sendable, Hashable {
    public let sources: [SourceEvidence]
    public let shortfall: [ShortfallFact]

    init(_ ffi: FfiAcquisitionEvidence) {
        sources = ffi.sources.map(SourceEvidence.init)
        shortfall = ffi.shortfall.map(ShortfallFact.init)
    }
}

/// One `NMPQuery` element: the full current snapshot (never a bare delta --
/// `NMPQuery` folds unbounded delta streams and retains windowed
/// authoritative frames, see that type's doc) plus the query's current
/// scoped acquisition evidence.
public struct RowBatch: Sendable {
    public let rows: [Row]
    public let evidence: AcquisitionEvidence
    /// The window's mechanical growth fact (#485). Always present on a
    /// windowed observation; `nil` on an unbounded one -- there is no
    /// window whose growth could be reported, exactly as
    /// `NMPQuery.requestRows(atLeast:)` refuses with `.unwindowed` there.
    public let load: WindowLoad?
}
