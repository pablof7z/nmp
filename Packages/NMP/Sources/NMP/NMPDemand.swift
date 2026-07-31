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
/// The Rust constructor canonicalizes the branches -- sorted, exact
/// duplicates collapsed -- so permuted or repeated input yields the same
/// observation and the same per-branch evidence order.
public struct NMPLiveQuery: Sendable, Hashable {
    /// The demand branches. Must be nonempty and at most `maxBranches`;
    /// both are typed refusals at `observe`, never silent truncation.
    public var branches: [NMPDemand]
    /// Bound on the MERGED row union, applied after branch rows are merged by
    /// event id -- never `N` rows per branch. Distinct from a branch's own
    /// `NMPFilter.limit`, which bounds only that branch's selection.
    public var aggregateResultLimit: UInt32?

    public init(branches: [NMPDemand], aggregateResultLimit: UInt32? = nil) {
        self.branches = branches
        self.aggregateResultLimit = aggregateResultLimit
    }

    /// The hard ceiling on branches in one observation. Exceeding it refuses
    /// the whole declaration.
    public static var maxBranches: UInt32 { maxQueryBranches() }
}

extension NMPLiveQuery {
    func toFfi() -> FfiLiveQuery {
        FfiLiveQuery(
            branches: branches.map { $0.toFfi() },
            aggregateResultLimit: aggregateResultLimit
        )
    }

    init(_ ffi: FfiLiveQuery) {
        self.init(
            branches: ffi.branches.map(NMPDemand.init),
            aggregateResultLimit: ffi.aggregateResultLimit
        )
    }
}
