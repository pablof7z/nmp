// NIP-51 Simple groups: tolerant, observational parsing (#863).
//
// A `Row` handed to this file may have been constructed by the app itself --
// any kind, any signature, no sources. So the parser is deliberately
// tolerant and its result is deliberately plain data. There is no
// observation-qualified wrapper, projection-error family, or frame proof
// here, and `scripts/check-nip51-no-derived-authority.sh` fails the build if
// one is reintroduced.
//
// Reading kind:10009 stays the ordinary demand/observation noun
// (`activeAccountDemand()` in NIP29.swift). Browsing a NIP-29 group still
// takes a host the app explicitly chose -- see `groupDiscoveryDemand(host:)`.

import NMPFFI

/// One tolerantly parsed Simple-groups entry -- group id, host relay,
/// optional display name.
///
/// `hostRelay` is a canonically spelled *observed* string. It is not a
/// routing permission: passing it to `groupDiscoveryDemand(host:)` is the
/// app's own explicit decision, and that function parses it like any other
/// caller-supplied host.
public struct SimpleGroupEntry: Sendable, Hashable {
    public let groupId: String
    public let hostRelay: String
    public let name: String?

    init(_ ffi: FfiSimpleGroupEntry) {
        groupId = ffi.groupId
        hostRelay = ffi.hostRelay
        name = ffi.name
    }
}

/// Tolerantly parsed Simple groups data. Item and relay ordering is
/// preserved. Malformed public items and encrypted private content remain
/// explicit evidence rather than disappearing at the native boundary.
///
/// This value is **observational only**. It may have been produced from a
/// wholly caller-constructed `Row` of any kind, and it grants no signature,
/// canonical-store, provenance, routing, or mutation authority.
public struct SimpleGroupsList: Sendable, Hashable {
    public let items: [SimpleGroupEntry]
    public let relaysInUse: [String]
    public let malformedItemCount: UInt64
    public let hasPrivateContent: Bool

    init(_ ffi: FfiSimpleGroupsList) {
        items = ffi.items.map(SimpleGroupEntry.init)
        relaysInUse = ffi.relaysInUse
        malformedItemCount = ffi.malformedItemCount
        hasPrivateContent = ffi.hasPrivateContent
    }
}

/// Tolerantly parse Simple-groups-shaped public items from an untrusted
/// `Row` (#863). Infallible and kind-agnostic: malformed individual items
/// are counted, never fatal, and the row's `kind`/`sig` are not consulted.
///
/// The result is observational data only.
public func parseSimpleGroupsListTolerant(_ row: Row) -> SimpleGroupsList {
    let ffiRow = FfiRow(
        id: row.id, pubkey: row.pubkey, createdAt: row.createdAt, kind: row.kind,
        tags: row.tags, content: row.content, sig: row.sig, sources: row.sources
    )
    return SimpleGroupsList(NMPFFI.parseSimpleGroupsListTolerant(row: ffiRow))
}
