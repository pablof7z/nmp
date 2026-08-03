// The explicit live-query identity, in ergonomic Swift shape (M4 plan §9,
// #107). `NMPEngine.observe(_ filter: NMPFilter)` still applies the static
// AuthorOutboxes/Public default (`nmp_grammar::Demand::from_filter`); a dev
// reaches for `NMPDemand` once that default isn't enough -- declaring
// `.pinned` wire authority or a non-`.agnostic` cache mode.

import NMPFFI

/// Which authority resolves a query's relay set (`nmp_grammar::
/// SourceAuthority` mirror, #107).
public enum NMPSourceAuthority: Sendable, Hashable {
    case authorOutboxes
    case `public`
    /// Ask ONLY this relay set, on the wire, full stop -- never neutral
    /// author facts, hints, provenance, or operator policy, regardless of
    /// whether the selection is author-bearing. Must be nonempty:
    /// `NMPEngine.observe(_ demand:)` throws `NMPError.emptyPinnedRelaySet`
    /// if it is not.
    case pinned(Set<String>)
}

/// `nmp_grammar::AccessContext` mirror. Closed vocabulary: an unauthenticated
/// `public` connection, or NIP-42 authentication against one stable expected
/// public key (hex). The `nip42` identity is frozen in the demand; changing
/// the active account never redirects it (#8).
public enum NMPAccessContext: Sendable, Hashable {
    case `public`
    case nip42(publicKey: String)
}

/// `nmp_grammar::CacheMode` mirror (#107). Meaningful only alongside
/// `NMPSourceAuthority.pinned` -- a no-op under any other source, since
/// there is no pinned relay set to intersect a cached row's provenance
/// against.
public enum NMPCacheMode: Sendable, Hashable {
    /// Serve every matching cached row regardless of provenance.
    case agnostic
    /// Serve only cached rows whose unioned provenance set intersects the
    /// pinned relay set.
    case strict
}

/// Per-handle coverage/wire policy (`nmp_grammar::Freshness`, #565).
/// Whole seconds match Nostr timestamp and coverage-watermark precision.
public enum NMPFreshness: Sendable, Hashable {
    case live
    case maxAge(seconds: UInt64)
    case cacheOnly
}

/// The full live-query declaration a dev supplies -- `selection + source +
/// access + cache + freshness` (`nmp_grammar::Demand` mirror, #106/#107/#565).
public struct NMPDemand: Sendable, Hashable {
    public var selection: NMPFilter
    public var source: NMPSourceAuthority
    public var access: NMPAccessContext
    public var cache: NMPCacheMode
    public var freshness: NMPFreshness

    public init(
        selection: NMPFilter,
        source: NMPSourceAuthority,
        access: NMPAccessContext = .public,
        cache: NMPCacheMode = .agnostic,
        freshness: NMPFreshness = .live
    ) {
        self.selection = selection
        self.source = source
        self.access = access
        self.cache = cache
        self.freshness = freshness
    }
}

// MARK: - Ergonomic -> Ffi

extension NMPSourceAuthority {
    func toFfi() -> FfiSourceAuthority {
        switch self {
        case .authorOutboxes: return .authorOutboxes
        case .public: return .public
        case .pinned(let relays): return .pinned(relays: Array(relays))
        }
    }

    init(_ ffi: FfiSourceAuthority) {
        switch ffi {
        case .authorOutboxes: self = .authorOutboxes
        case .public: self = .public
        case .pinned(let relays): self = .pinned(Set(relays))
        }
    }
}

extension NMPAccessContext {
    func toFfi() -> FfiAccessContext {
        switch self {
        case .public: return .public
        case let .nip42(publicKey): return .nip42(publicKey: publicKey)
        }
    }

    init(_ ffi: FfiAccessContext) {
        switch ffi {
        case .public: self = .public
        case let .nip42(publicKey): self = .nip42(publicKey: publicKey)
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
            source: source.toFfi(),
            access: access.toFfi(),
            cache: cache.toFfi(),
            freshness: freshness.toFfi()
        )
    }

    init(_ ffi: FfiDemand) {
        self.init(
            selection: NMPFilter(ffi.selection),
            source: NMPSourceAuthority(ffi.source),
            access: NMPAccessContext(ffi.access),
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
/// two hosts into one `.pinned([a, b])` produces a confidently wrong
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
