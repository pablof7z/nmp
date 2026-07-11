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

    init(_ ffi: FfiRow) {
        id = ffi.id
        pubkey = ffi.pubkey
        createdAt = ffi.createdAt
        kind = ffi.kind
        tags = ffi.tags
        content = ffi.content
        sig = ffi.sig
    }
}

/// A query's aggregate coverage (ledger #7's variant): whether the engine
/// can PROVE the visible rows are everything up to a point in time, or
/// whether that has not (yet) been established.
public enum Coverage: Sendable, Hashable {
    case completeUpTo(UInt64)
    case unknown

    init(_ ffi: FfiCoverage) {
        switch ffi {
        case .completeUpTo(let unixSeconds): self = .completeUpTo(unixSeconds)
        case .unknown: self = .unknown
        }
    }
}

/// One `NMPQuery` element: the full accumulated snapshot (never a bare
/// delta -- `NMPQuery` does the accumulation, see that type's doc) plus the
/// query's current coverage.
public struct RowBatch: Sendable {
    public let rows: [Row]
    public let coverage: Coverage
}
