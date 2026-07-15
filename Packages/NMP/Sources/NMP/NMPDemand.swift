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
    /// Ask ONLY this relay set, on the wire, full stop -- never author-
    /// outbox/directory/app/fallback/indexer routing, regardless of whether
    /// the selection is author-bearing. Must be nonempty:
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

/// The full live-query identity a dev declares -- `selection + source +
/// access + cache` (`nmp_grammar::Demand` mirror, #106/#107).
public struct NMPDemand: Sendable, Hashable {
    public var selection: NMPFilter
    public var source: NMPSourceAuthority
    public var access: NMPAccessContext
    public var cache: NMPCacheMode

    public init(
        selection: NMPFilter,
        source: NMPSourceAuthority,
        access: NMPAccessContext = .public,
        cache: NMPCacheMode = .agnostic
    ) {
        self.selection = selection
        self.source = source
        self.access = access
        self.cache = cache
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

extension NMPDemand {
    func toFfi() -> FfiDemand {
        FfiDemand(
            selection: selection.toFfi(),
            source: source.toFfi(),
            access: access.toFfi(),
            cache: cache.toFfi()
        )
    }

    init(_ ffi: FfiDemand) {
        self.init(
            selection: NMPFilter(ffi.selection),
            source: NMPSourceAuthority(ffi.source),
            access: NMPAccessContext(ffi.access),
            cache: NMPCacheMode(ffi.cache)
        )
    }
}
