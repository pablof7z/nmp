import NMP
import NMPFFI

/// The ordinary NMP observations used for one normalized reference. Only the
/// canonical demand supplies rendered truth; helper demands use trustworthy
/// hints to acquire into NMP's one store.
public struct NostrReferenceDemandPlan: Sendable, Hashable {
    public let targetKey: String
    public let canonical: NMPDemand
    public let helpers: [NMPDemand]

    public init(targetKey: String, canonical: NMPDemand, helpers: [NMPDemand]) {
        self.targetKey = targetKey
        self.canonical = canonical
        self.helpers = helpers
    }
}

public func referenceDemandPlan(for target: NostrReferenceTarget) -> NostrReferenceDemandPlan {
    let ffi = contentReferenceDemandPlan(target: target.ffiValue)
    return NostrReferenceDemandPlan(
        targetKey: ffi.targetKey,
        canonical: demand(from: ffi.canonical),
        helpers: ffi.helpers.map(demand(from:))
    )
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
    let tags = Dictionary(uniqueKeysWithValues: pairs)
    return NMPFilter(
        kinds: ffi.kinds,
        authors: ffi.authors.map(binding(from:)),
        ids: ffi.ids.map(binding(from:)),
        tags: tags,
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
    }
}

private func cache(from ffi: FfiCacheMode) -> NMPCacheMode {
    switch ffi {
    case .agnostic: return .agnostic
    case .strict: return .strict
    }
}
