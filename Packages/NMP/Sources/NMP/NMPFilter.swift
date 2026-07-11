// The two-noun query descriptor, in ergonomic Swift shape (M4 plan §9). A
// dev builds an `NMPFilter` value -- never an `FfiFilter`/`FfiBinding`
// directly -- and hands it to `NMPEngine.observe(_:)`. `NMPBinding` mirrors
// `nmp_grammar::Binding`'s four cases as a plain (indirect) Swift enum;
// `FfiDerived`/`FfiSetOp` (UniFFI objects, needed only because Rust enums
// can't self-reference) are constructed on the way OUT (`toFfi()`), never
// exposed to a caller.

import NMPFFI

/// The reactive identity root (VISION P3). `.activePubkey` is the only
/// variant today; additional identity fields are additive.
public enum NMPIdentityField: Sendable, Hashable {
    case activePubkey
}

/// The closed projection vocabulary a `.derived` binding projects through.
public enum NMPSelector: Sendable, Hashable {
    case authors
    case ids
    /// Exactly one character from the closed M1 tag-name set (`p e a d E t q`).
    case tag(Character)
    case addressCoord
}

/// Set algebra over resolved value sets (union/intersect/diff of two or more
/// bindings -- e.g. "follows minus mutes").
public enum NMPSetAlgebra: Sendable, Hashable {
    case union
    case intersect
    case diff
}

/// Every bindable filter-field value. `indirect` because `.derived` embeds a
/// whole `NMPFilter` (which itself may embed further bindings).
public indirect enum NMPBinding: Sendable, Hashable {
    /// A fixed, literal set of hex values (pubkeys/ids/tag values).
    case literal(Set<String>)
    /// Re-resolves reactively whenever the named identity field changes
    /// (e.g. the active account).
    case reactive(NMPIdentityField)
    /// Projects `inner`'s matching rows through `project` (e.g. "authors of
    /// my kind:3 contact list, projected through their `p` tags" = follows).
    case derived(inner: NMPFilter, project: NMPSelector)
    /// Combines several bindings with a set operation.
    case setOp(NMPSetAlgebra, [NMPBinding])
}

/// A live-query filter whose field values may be reactive `NMPBinding`s
/// (`nmp_grammar::Filter` mirror, Swift-shaped). Values, not code: an
/// `NMPFilter` is `Hashable`/`Sendable` and re-`observe`-able at will --
/// editing a filter is constructing a new value, never mutating a running
/// query in place.
public struct NMPFilter: Sendable, Hashable {
    public var kinds: [UInt16]?
    public var authors: NMPBinding?
    public var ids: NMPBinding?
    public var tags: [Character: NMPBinding]
    public var since: UInt64?
    public var until: UInt64?
    public var limit: UInt32?

    public init(
        kinds: [UInt16]? = nil,
        authors: NMPBinding? = nil,
        ids: NMPBinding? = nil,
        tags: [Character: NMPBinding] = [:],
        since: UInt64? = nil,
        until: UInt64? = nil,
        limit: UInt32? = nil
    ) {
        self.kinds = kinds
        self.authors = authors
        self.ids = ids
        self.tags = tags
        self.since = since
        self.until = until
        self.limit = limit
    }
}

// MARK: - Ergonomic -> Ffi

extension NMPSelector {
    func toFfi() -> FfiSelector {
        switch self {
        case .authors: return .authors
        case .ids: return .ids
        case .tag(let c): return .tag(name: String(c))
        case .addressCoord: return .addressCoord
        }
    }

    init(_ ffi: FfiSelector) {
        switch ffi {
        case .authors: self = .authors
        case .ids: self = .ids
        case .tag(let name): self = .tag(name.first ?? " ")
        case .addressCoord: self = .addressCoord
        }
    }
}

extension NMPIdentityField {
    func toFfi() -> FfiIdentityField {
        switch self {
        case .activePubkey: return .activePubkey
        }
    }

    init(_ ffi: FfiIdentityField) {
        switch ffi {
        case .activePubkey: self = .activePubkey
        }
    }
}

extension NMPSetAlgebra {
    func toFfi() -> FfiSetAlgebra {
        switch self {
        case .union: return .union
        case .intersect: return .intersect
        case .diff: return .diff
        }
    }

    init(_ ffi: FfiSetAlgebra) {
        switch ffi {
        case .union: self = .union
        case .intersect: self = .intersect
        case .diff: self = .diff
        }
    }
}

extension NMPBinding {
    func toFfi() -> FfiBinding {
        switch self {
        case .literal(let values):
            return .literal(values: Array(values))
        case .reactive(let field):
            return .reactive(field: field.toFfi())
        case .derived(let inner, let project):
            return .derived(derived: FfiDerived(inner: inner.toFfi(), project: project.toFfi()))
        case .setOp(let op, let operands):
            return .setOp(setOp: FfiSetOp(op: op.toFfi(), operands: operands.map { $0.toFfi() }))
        }
    }

    init(_ ffi: FfiBinding) {
        switch ffi {
        case .literal(let values):
            self = .literal(Set(values))
        case .reactive(let field):
            self = .reactive(NMPIdentityField(field))
        case .derived(let derived):
            self = .derived(inner: NMPFilter(derived.inner()), project: NMPSelector(derived.project()))
        case .setOp(let setOp):
            self = .setOp(NMPSetAlgebra(setOp.op()), setOp.operands().map { NMPBinding($0) })
        }
    }
}

extension NMPFilter {
    func toFfi() -> FfiFilter {
        FfiFilter(
            kinds: kinds,
            authors: authors?.toFfi(),
            ids: ids?.toFfi(),
            tags: Dictionary(uniqueKeysWithValues: tags.map { (String($0.key), $0.value.toFfi()) }),
            since: since,
            until: until,
            limit: limit
        )
    }

    init(_ ffi: FfiFilter) {
        self.init(
            kinds: ffi.kinds,
            authors: ffi.authors.map(NMPBinding.init),
            ids: ffi.ids.map(NMPBinding.init),
            tags: Dictionary(uniqueKeysWithValues: ffi.tags.map { (Character($0.key), NMPBinding($0.value)) }),
            since: ffi.since,
            until: ffi.until,
            limit: ffi.limit
        )
    }
}
