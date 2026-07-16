import NMP
import NMPContent
import NMPFFI

/// The ordinary NMP observations a selected reference component may choose
/// to open. This is a pure value; it owns no engine, handle, or lifecycle.
public struct NostrReferenceDemandPlan: Sendable, Hashable {
    public let targetKey: String
    public let canonical: NMPDemand
    public let helpers: [NMPDemand]
    public let discardedRelayHints: UInt32

    public init(
        targetKey: String,
        canonical: NMPDemand,
        helpers: [NMPDemand],
        discardedRelayHints: UInt32
    ) {
        self.targetKey = targetKey
        self.canonical = canonical
        self.helpers = helpers
        self.discardedRelayHints = discardedRelayHints
    }
}

/// Ask the Rust grammar owner to validate and lower one authored reference.
/// The selected component calls this helper; parsing does not.
public func referenceDemandPlan(
    for target: NostrReferenceTarget
) throws -> NostrReferenceDemandPlan {
    let ffi = try referenceDemandPlan(target: ffiTarget(from: target))
    return NostrReferenceDemandPlan(
        targetKey: ffi.targetKey,
        canonical: demand(from: ffi.canonical),
        helpers: ffi.helpers.map(demand(from:)),
        discardedRelayHints: ffi.discardedRelayHints
    )
}

private func ffiTarget(from target: NostrReferenceTarget) -> FfiReferenceTarget {
    switch target {
    case .profile(let pubkey, let relayHints):
        return .profile(pubkey: pubkey, relayHints: relayHints)
    case .event(let id, let authorHint, let kindHint, let relayHints):
        return .event(
            id: id,
            authorHint: authorHint,
            kindHint: kindHint,
            relayHints: relayHints
        )
    case .address(let kind, let author, let identifier, let relayHints):
        return .address(
            kind: kind,
            author: author,
            identifier: identifier,
            relayHints: relayHints
        )
    }
}

private func demand(from ffi: FfiDemand) -> NMPDemand {
    NMPDemand(
        selection: filter(from: ffi.selection),
        source: source(from: ffi.source),
        access: access(from: ffi.access),
        cache: cache(from: ffi.cache)
    )
}

private func filter(from ffi: FfiFilter) -> NMPFilter {
    let pairs: [(Character, NMPBinding)] = ffi.tags.compactMap { key, value in
        guard key.count == 1, let character = key.first else { return nil }
        return (character, binding(from: value))
    }
    return NMPFilter(
        kinds: ffi.kinds,
        authors: ffi.authors.map(binding(from:)),
        ids: ffi.ids.map(binding(from:)),
        tags: Dictionary(uniqueKeysWithValues: pairs),
        since: ffi.since,
        until: ffi.until,
        limit: ffi.limit
    )
}

private func binding(from ffi: FfiBinding) -> NMPBinding {
    switch ffi {
    case .literal(let values):
        return .literal(Set(values))
    case .reactive(let field):
        switch field {
        case .activePubkey: return .reactive(.activePubkey)
        }
    case .derived(let derived):
        return .derived(
            inner: filter(from: derived.inner()),
            project: selector(from: derived.project())
        )
    case .setOp(let setOp):
        return .setOp(
            setAlgebra(from: setOp.op()),
            setOp.operands().map(binding(from:))
        )
    }
}

private func selector(from ffi: FfiSelector) -> NMPSelector {
    switch ffi {
    case .authors: return .authors
    case .ids: return .ids
    case .tag(let name): return .tag(name)
    case .addressCoord: return .addressCoord
    }
}

private func setAlgebra(from ffi: FfiSetAlgebra) -> NMPSetAlgebra {
    switch ffi {
    case .union: return .union
    case .intersect: return .intersect
    case .diff: return .diff
    }
}

private func source(from ffi: FfiSourceAuthority) -> NMPSourceAuthority {
    switch ffi {
    case .authorOutboxes: return .authorOutboxes
    case .public: return .public
    case .pinned(let relays): return .pinned(Set(relays))
    }
}

private func access(from ffi: FfiAccessContext) -> NMPAccessContext {
    switch ffi {
    case .public: return .public
    case let .nip42(publicKey): return .nip42(publicKey: publicKey)
    }
}

private func cache(from ffi: FfiCacheMode) -> NMPCacheMode {
    switch ffi {
    case .agnostic: return .agnostic
    case .strict: return .strict
    }
}
