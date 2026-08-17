// The live-query identity, in ergonomic Swift shape (M4 plan §9, #107).
// Every read declares one: `NMPEngine.observe(_:)` takes an `NMPLiveQuery`
// whose branches are `NMPDemand`s. No door infers routing from the
// selection's shape (#847) -- but the routing an app gets by saying nothing
// is `.auto`, so saying nothing is the ordinary way to read.

import NMPFFI

/// Where a query's reads come from (`nmp_grammar::ReadRouting` mirror). A
/// strategy, not a resolved relay set: re-executed against whatever the
/// engine knows at each moment.
///
/// These two words are the whole app-facing routing vocabulary, for reads
/// and writes alike -- see `NMPWriteRouting`.
public enum NMPReadRouting: Sendable, Hashable {
    /// "Figure out where to read this from." NIP-65 outbound relays for
    /// every author the selection resolves, relay hints and prior
    /// provenance, then the operator's app and fallback lanes.
    ///
    /// The default. Naming a routing value is what an app does to OVERRIDE
    /// NMP, never what it must do to use NMP.
    case auto
    /// Ask ONLY this relay set, on the wire, full stop -- never widened to
    /// outbox, directory, app, fallback or indexer relays, regardless of
    /// whether the selection is author-bearing. Must be nonempty:
    /// `NMPEngine.observe(_:)` throws `NMPError.emptyExplicitRelaySet` if it
    /// is not.
    case explicit([String])
}

/// `nmp_grammar::CacheMode` mirror (#107). Meaningful only alongside
/// `NMPReadRouting.explicit` -- a no-op under `.auto`, since there is no
/// explicit relay set to intersect a cached row's provenance against.
public enum NMPCacheMode: Sendable, Hashable {
    /// Serve every matching cached row regardless of provenance.
    case agnostic
    /// Serve only cached rows whose unioned provenance set intersects the
    /// explicit relay set.
    case strict
}

/// Per-handle coverage/wire policy (`nmp_grammar::Freshness`, #565).
/// Whole seconds match Nostr timestamp and coverage-watermark precision.
public enum NMPFreshness: Sendable, Hashable {
    case live
    case maxAge(seconds: UInt64)
    case cacheOnly
}

/// The full live-query declaration a dev supplies -- `selection + routing +
/// authenticateAs + cache + freshness` (`nmp_grammar::Demand` mirror,
/// #106/#107/#565).
///
/// Every parameter but `selection` defaults, so the ordinary declaration is
/// the selection and nothing else:
///
/// ```swift
/// NMPDemand(selection: filter)   // routing: .auto
/// ```
public struct NMPDemand: Sendable, Hashable {
    public var selection: NMPFilter
    /// Where this demand's reads come from. Defaults to `.auto`: an app that
    /// says nothing gets NMP's routing.
    public var routing: NMPReadRouting
    /// The identity these reads authenticate as, as a 32-byte hex public
    /// key. `nil` -- the default and the ordinary case -- reads on the
    /// connection bound to no identity, which today never authenticates: a
    /// relay's NIP-42 challenge on such a connection is currently dropped
    /// rather than routed to the installed `NMPAuthPolicy` (issue #1889).
    /// A non-nil key pins these reads to a session that authenticates as it.
    public var authenticateAs: String?
    public var cache: NMPCacheMode
    public var freshness: NMPFreshness

    public init(
        selection: NMPFilter,
        routing: NMPReadRouting = .auto,
        authenticateAs: String? = nil,
        cache: NMPCacheMode = .agnostic,
        freshness: NMPFreshness = .live
    ) {
        self.selection = selection
        self.routing = routing
        self.authenticateAs = authenticateAs
        self.cache = cache
        self.freshness = freshness
    }
}

// MARK: - Ergonomic -> Ffi

extension NMPReadRouting {
    func toFfi() -> FfiReadRouting {
        switch self {
        case .auto: return .auto
        case .explicit(let relays): return .explicit(relays: relays)
        }
    }

    init(_ ffi: FfiReadRouting) {
        switch ffi {
        case .auto: self = .auto
        case .explicit(let relays): self = .explicit(relays)
        }
    }
}

extension NMPCacheMode {
    func toFfi() -> FfiCacheMode {
        switch self {
        case .agnostic: return .agnostic
        case .strict: return .strict
        }
    }

    init(_ ffi: FfiCacheMode) {
        switch ffi {
        case .agnostic: self = .agnostic
        case .strict: self = .strict
        }
    }
}

extension NMPFreshness {
    func toFfi() -> FfiFreshness {
        switch self {
        case .live: return .live
        case let .maxAge(seconds): return .maxAge(seconds: seconds)
        case .cacheOnly: return .cacheOnly
        }
    }

    init(_ ffi: FfiFreshness) {
        switch ffi {
        case .live: self = .live
        case let .maxAge(seconds): self = .maxAge(seconds: seconds)
        case .cacheOnly: self = .cacheOnly
        }
    }
}

extension NMPDemand {
    func toFfi() -> FfiDemand {
        FfiDemand(
            selection: selection.toFfi(),
            routing: routing.toFfi(),
            authenticateAs: authenticateAs,
            cache: cache.toFfi(),
            freshness: freshness.toFfi()
        )
    }

    init(_ ffi: FfiDemand) {
        self.init(
            selection: NMPFilter(ffi.selection),
            routing: NMPReadRouting(ffi.routing),
            authenticateAs: ffi.authenticateAs,
            cache: NMPCacheMode(ffi.cache),
            freshness: NMPFreshness(ffi.freshness)
        )
    }
}

/// One live-query declaration (#1108): one or more complete, independent
/// `NMPDemand` branches observed through ONE lifecycle, plus the optional
/// bound on their merged row union.
///
/// Some correct reads need several branches whose results form one semantic
/// query and whose host-scoped values must not cross between them. Flattening
/// two hosts into one `.explicit([a, b])` produces a confidently wrong
/// cross-product; handing an app a list of demands makes the app own the
/// aggregate observation. This is neither: it is one read noun.
///
/// A value's branches are always canonical -- sorted, exact duplicates
/// collapsed, never empty -- because `single` and `union` are the only ways to
/// make one, and `union` canonicalizes through the same Rust construction the
/// engine itself uses. Two declarations of the same branches are therefore one
/// value with one hash whatever order they were typed in, `branches` is
/// exactly what the observation will open, and `branches[i]` names the branch
/// each delivered `RowBatch.evidence[i]` reports on.
public struct NMPLiveQuery: Sendable, Hashable {
    private let canonicalBranches: [NMPDemand]
    private let mergedRowBound: UInt32?

    private init(canonicalBranches: [NMPDemand], mergedRowBound: UInt32?) {
        self.canonicalBranches = canonicalBranches
        self.mergedRowBound = mergedRowBound
    }

    /// The canonical demand branches, in the one order this observation's
    /// per-branch evidence is indexed by. Never empty.
    public var branches: [NMPDemand] { canonicalBranches }

    /// Bound on the MERGED row union, applied after branch rows are merged by
    /// event id -- never `N` rows per branch. Distinct from a branch's own
    /// `NMPFilter.limit`, which bounds only that branch's selection.
    public var aggregateResultLimit: UInt32? { mergedRowBound }

    /// One complete demand observed on its own -- already canonical, so this
    /// cannot be refused. Identical in lifecycle, frame shape and evidence
    /// shape to a union of one.
    public static func single(_ branch: NMPDemand) -> NMPLiveQuery {
        NMPLiveQuery(canonicalBranches: [branch], mergedRowBound: nil)
    }

    /// Compose independent live queries into ONE canonical declaration.
    ///
    /// Inputs flatten, duplicates collapse and order is canonicalized, so
    /// permutations of the same inputs are the same value.
    /// `aggregateResultLimit` bounds the merged row union globally.
    ///
    /// Throws `NMPError.emptyQueryUnion` for no branches at all,
    /// `.aggregateResultLimitZero` for a bound that can never contain a row,
    /// `.nestedAggregateResultLimit` when an input already carries its own
    /// aggregate bound, and `.tooManyQueryBranches` above `maxBranches` --
    /// the whole declaration is refused, never a silently installed subset.
    public static func union(
        _ branches: [NMPLiveQuery],
        aggregateResultLimit: UInt32? = nil
    ) throws -> NMPLiveQuery {
        NMPLiveQuery(try nmpRethrowing {
            try liveQueryUnion(
                branches: branches.map { $0.toFfi() },
                aggregateResultLimit: aggregateResultLimit
            )
        })
    }

    /// The hard ceiling on branches in one observation. Exceeding it refuses
    /// the whole declaration.
    public static var maxBranches: UInt32 { maxQueryBranches() }
}

extension NMPLiveQuery {
    func toFfi() -> FfiLiveQuery {
        FfiLiveQuery(
            branches: canonicalBranches.map { $0.toFfi() },
            aggregateResultLimit: mergedRowBound
        )
    }

    /// Lift a live query Rust already built (`union`'s own result, and every
    /// protocol-composed read such as `NMPGroup.read`). It is canonical
    /// by construction there, so this never re-canonicalizes -- and being
    /// non-public, it is not a door an app can forge a noncanonical value
    /// through.
    init(_ ffi: FfiLiveQuery) {
        self.init(
            canonicalBranches: ffi.branches.map(NMPDemand.init),
            mergedRowBound: ffi.aggregateResultLimit
        )
    }
}
