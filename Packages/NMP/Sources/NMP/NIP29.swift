// The read-only NIP-29 host-browser projection (#108) -- pure functions,
// same shape as `decodeNostrEntity` (#116): no `NMPEngine` instance is
// needed to call any of these. Pass the returned `NMPDemand` straight to
// `NMPEngine.observe(_ demand:)`, exactly like any other `NMPDemand`.
//
// `groupSendIntent`/`GroupSendIntent` (#115) are this file's write-side
// counterpart: an app couriers `Row`s it already has from a live
// `groupContentDemand` read, and `nmp-nip29::compose_group_send` owns 100%
// of the `h`/`previous` tag composition -- the app never sees either tag,
// routing, or host authority directly, only the opaque, take-once
// `GroupSendIntent` `NMPEngine.publishComposed(_:)` consumes.

import NMPFFI

/// A remembered NIP-29 group reference (#108, `FfiGroupRef` mirror) --
/// group id, host relay, and optional display name.
public struct GroupRef: Sendable, Hashable {
    public let groupId: String
    public let host: String
    public let name: String?

    init(_ ffi: FfiGroupRef) {
        groupId = ffi.groupId
        host = ffi.host
        name = ffi.name
    }
}

/// The composed remembered-groups/host-relays value (#108,
/// `FfiRememberedGroups` mirror) -- what `decodeRememberedGroups(_:)`
/// returns from a delivered kind:10009 `Row`.
public struct RememberedGroups: Sendable, Hashable {
    public let groups: [GroupRef]
    public let hostsInUse: [String]
    public let hasPrivateContent: Bool

    init(_ ffi: FfiRememberedGroups) {
        groups = ffi.groups.map(GroupRef.init)
        hostsInUse = ffi.hostsInUse
        hasPrivateContent = ffi.hasPrivateContent
    }
}

/// The signed-in account's remembered-groups demand (#108): `kinds:
/// [10009]`, `AuthorOutboxes + Public`. Signed-out (no active account)
/// resolves to zero rows through the ordinary reactive-binding empty-
/// resolution path -- no special case needed on the caller's side.
public func activeAccountDemand() -> NMPDemand {
    NMPDemand(NMPFFI.activeAccountDemand())
}

/// Group discovery (kind:39000) pinned to `host` (#108). Throws
/// `NMPError.invalidRelayUrl` if `host` doesn't parse.
public func groupDiscoveryDemand(host: String) throws -> NMPDemand {
    try NMPDemand(nmpRethrowing { try NMPFFI.groupDiscoveryDemand(host: host) })
}

/// Group content (kinds 9, 30315), `h`-tag scoped to `groupId`, pinned to
/// `host` (#108). Throws `NMPError.invalidRelayUrl` if `host` doesn't
/// parse.
public func groupContentDemand(host: String, groupId: String) throws -> NMPDemand {
    try NMPDemand(
        nmpRethrowing { try NMPFFI.groupContentDemand(host: host, groupId: groupId) }
    )
}

/// Decode a delivered kind:10009 `Row` into the composed remembered-
/// groups/host-relays value (#108). Infallible: malformed individual items
/// are dropped internally, never the whole decode.
public func decodeRememberedGroups(_ row: Row) -> RememberedGroups {
    let ffiRow = FfiRow(
        id: row.id, pubkey: row.pubkey, createdAt: row.createdAt, kind: row.kind,
        tags: row.tags, content: row.content, sig: row.sig, sources: row.sources
    )
    return RememberedGroups(NMPFFI.decodeRememberedGroups(row: ffiRow))
}

/// A composed NIP-29 group send (#115), returned by `groupSendIntent`.
/// Opaque and take-once -- pass it to `NMPEngine.publishComposed(_:)`
/// exactly once; a second attempt throws `NMPError.intentAlreadyConsumed`.
/// Never exposes `h`, `previous`, routing, or host authority: this crate
/// composed all of that internally from the couriered `recentRows`.
public struct GroupSendIntent: Sendable {
    let ffi: FfiComposedWriteIntent
}

/// Compose a NIP-29 group send (#115): `recentRows` are delivered kind:9/
/// 30315 `Row`s the app is already rendering from its own live
/// `groupContentDemand` read (#108) -- couriered, not hand-rolled (see
/// `nmp_nip29::compose_group_send`'s own doc for that distinction). This
/// function owns 100% of the `h`/`previous` tag
/// selection/verification/truncation/encoding; the app supplies only the
/// primitives it already has. `kind` is entirely the caller's choice --
/// this call (and everything it reaches) is kind-blind. Publish the
/// result via `NMPEngine.publishComposed(_:)`.
public func groupSendIntent(
    host: String,
    groupId: String,
    authorPubkey: String,
    createdAt: UInt64,
    kind: UInt16,
    content: String,
    extraTags: [[String]] = [],
    recentRows: [Row] = []
) throws -> GroupSendIntent {
    let ffiRows = recentRows.map {
        FfiRow(
            id: $0.id, pubkey: $0.pubkey, createdAt: $0.createdAt, kind: $0.kind,
            tags: $0.tags, content: $0.content, sig: $0.sig, sources: $0.sources
        )
    }
    return try GroupSendIntent(
        ffi: nmpRethrowing {
            try NMPFFI.groupSendIntent(
                host: host, groupId: groupId, authorPubkey: authorPubkey, createdAt: createdAt,
                kind: kind, content: content, extraTags: extraTags, recentRows: ffiRows
            )
        }
    )
}
