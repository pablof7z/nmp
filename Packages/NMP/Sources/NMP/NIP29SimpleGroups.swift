// NIP-51 Simple groups, exposed with the NIP-29 product capability:
// tolerant, observational parsing (#863/#1551).
//
// A `Row` handed to this file may have been constructed by the app itself --
// any kind, any signature, no sources. So the parser is deliberately
// tolerant and its result is deliberately plain data. There is no
// observation-qualified wrapper, projection-error family, or frame proof
// here, and `scripts/check-nip29-group-list-ownership.sh` fails the build if
// one is reintroduced.
//
// Reading kind:10009 stays the ordinary demand/observation noun
// (`currentAccountGroupListDemand()`, below). Typed add/remove methods compile
// through Rust's durable semantic operation and return the ordinary `Receipt`.
// Browsing a NIP-29 group still takes an explicit, caller-supplied relay set --
// see `NMPRelayScope.on(_:)`.
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

/// A typed group-list action was refused before ordinary receipt custody.
public enum GroupListActionError: Error, Sendable, Equatable {
    case invalidRelayUrl(got: String)
    case automaticRoutingUnavailable
    case signedOut
    case engineClosed
    case receiptUnavailable

    init(_ ffi: FfiGroupListActionError) {
        switch ffi {
        case .InvalidRelayUrl(let got): self = .invalidRelayUrl(got: got)
        case .AutomaticRoutingUnavailable: self = .automaticRoutingUnavailable
        case .SignedOut: self = .signedOut
        case .EngineClosed: self = .engineClosed
        case .ReceiptUnavailable: self = .receiptUnavailable
        }
    }
}

/// The signed-in account's Simple-groups-list demand (#108): `kinds:
/// [10009]`, `AuthorOutboxes + Public`. Signed-out (no current account)
/// resolves to zero rows through the ordinary reactive-binding empty-
/// resolution path -- no special case needed on the caller's side.
///
/// #1551 places this NIP-51-defined list with the NIP-29 product capability
/// that consumes it, without changing which NIP defines kind:10009.
public func currentAccountGroupListDemand() -> NMPDemand {
    NMPDemand(NMPFFI.currentAccountGroupListDemand())
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

public extension NMPEngine {
    /// Add one exact `(group id, canonical host)` identity through the
    /// ordinary durable write receipt. An existing display name is not
    /// rewritten, and the host carried by the list is not a publish route.
    func addGroupToList(
        groupId: String,
        hostRelay: String,
        name: String? = nil
    ) throws -> Receipt {
        try groupListReceipt {
            try ffi.addGroupToList(groupId: groupId, hostRelay: hostRelay, name: name)
        }
    }

    /// Remove every valid public group tag with this exact identity.
    func removeGroupFromList(groupId: String, hostRelay: String) throws -> Receipt {
        try groupListReceipt {
            try ffi.removeGroupFromList(groupId: groupId, hostRelay: hostRelay)
        }
    }

    /// Add one canonical relay-in-use tag without changing group tags.
    func addRelayInUse(_ relay: String) throws -> Receipt {
        try groupListReceipt { try ffi.addRelayInUse(relay: relay) }
    }

    /// Remove every valid equivalent relay-in-use tag without changing group
    /// tags or malformed evidence.
    func removeRelayInUse(_ relay: String) throws -> Receipt {
        try groupListReceipt { try ffi.removeRelayInUse(relay: relay) }
    }

    private func groupListReceipt(
        _ action: () throws -> NmpReceiptStream
    ) throws -> Receipt {
        do {
            return Receipt(handle: try action())
        } catch let error as FfiGroupListActionError {
            throw GroupListActionError(error)
        }
    }
}
