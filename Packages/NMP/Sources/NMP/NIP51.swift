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
// (`activeAccountDemand()`, below). Browsing a NIP-29 group still takes an
// explicit, caller-supplied relay set -- see `NMPRelayScope.on(_:)`.
//
// `SimpleGroupsList` is also the ONE native shape a decoded kind:10009 list
// takes (#858). The NIP-29-facing wrapper family that used to sit beside it
// merely renamed these fields and dropped `malformedItemCount`; there is no
// second projection of this value anywhere in the SDK.

import NMPFFI

/// One tolerantly parsed Simple-groups entry -- group id, host relay,
/// optional display name.
///
/// `hostRelay` is a canonically spelled *observed* string. It is not a
/// routing permission: passing it to `NMPRelayScope.on(_:)` is the app's own
/// explicit decision, and that constructor parses it like any other
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

/// The signed-in account's Simple-groups-list demand (#108): `kinds:
/// [10009]`, `AuthorOutboxes + Public`. Signed-out (no active account)
/// resolves to zero rows through the ordinary reactive-binding empty-
/// resolution path -- no special case needed on the caller's side.
///
/// #858 moved this out of NIP29.swift: kind:10009 is NIP-51's kind, so its
/// demand constructor lives with the rest of NIP-51.
public func activeAccountDemand() -> NMPDemand {
    NMPDemand(NMPFFI.activeAccountDemand())
}

/// Tolerantly parse Simple-groups-shaped public items from an untrusted
/// `Row` (#863). Infallible and kind-agnostic: malformed individual items
/// are counted, never fatal, and the row's `kind`/signature are not consulted.
///
/// The result is observational data only.
public func parseSimpleGroupsListTolerant(_ row: Row) -> SimpleGroupsList {
    let ffiRow = FfiRow(
        id: row.id, pubkey: row.pubkey, createdAt: row.createdAt, kind: row.kind,
        tags: row.tags, content: row.content, signature: row.signature.ffi, sources: row.sources
    )
    return SimpleGroupsList(NMPFFI.parseSimpleGroupsListTolerant(row: ffiRow))
}
