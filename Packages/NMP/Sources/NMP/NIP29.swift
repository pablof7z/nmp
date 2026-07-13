// The read-only NIP-29 host-browser projection (#108) -- pure functions,
// same shape as `decodeNostrEntity` (#116): no `NMPEngine` instance is
// needed to call any of these. Pass the returned `NMPDemand` straight to
// `NMPEngine.observe(_ demand:)`, exactly like any other `NMPDemand`.
//
// `NMPEngine.groupMessageIntent`/`GroupSendIntent` (#156) are this file's
// write-side counterpart. The app supplies semantic composer state; NMP owns
// author/time/kind, NIP-27 mention materialization, `p`/reply-`e` tags, and
// the existing `h`/`previous`/pinned-host composition.

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

/// A direct reply parent for a kind:9 group message. NMP turns this into the
/// marked reply `e` row plus the author's deduplicated recipient `p` row.
public struct GroupReplyParent: Sendable, Hashable {
    public let eventID: String
    public let authorPubkey: String

    public init(eventID: String, authorPubkey: String) {
        self.eventID = eventID
        self.authorPubkey = authorPubkey
    }

    func toFfi() -> FfiGroupReplyParent {
        FfiGroupReplyParent(eventId: eventID, authorPubkey: authorPubkey)
    }
}

/// A composed NIP-29 group message (#156), returned by
/// `NMPEngine.groupMessageIntent`.
/// Opaque and take-once -- pass it to `NMPEngine.publishComposed(_:)`
/// exactly once; a second attempt throws `NMPError.intentAlreadyConsumed`.
/// Never exposes the materialized tags, routing, author, or timestamp.
public struct GroupSendIntent: Sendable {
    let ffi: FfiComposedWriteIntent
}

extension NMPEngine {
    /// Compose an ordinary kind:9 group message from the state a native
    /// composer actually owns. `recipients` retain selection order; NMP
    /// deduplicates them, prefixes their `nostr:npub…` references to
    /// `content`, and emits matching `p` rows. `reply` contributes the marked
    /// direct-parent `e` row and its author recipient. `recentRows` are
    /// couriered only so NMP can derive NIP-29 `previous` evidence.
    ///
    /// The active account supplies the author and NMP supplies event time.
    /// The caller cannot choose a kind or inject raw tags. Publish the opaque
    /// result via `publishComposed(_:)`.
    public func groupMessageIntent(
        host: String,
        groupID: String,
        content: String,
        recipients: [String] = [],
        reply: GroupReplyParent? = nil,
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
                try ffi.groupMessageIntent(
                    host: host,
                    groupId: groupID,
                    content: content,
                    recipientPubkeys: recipients,
                    replyTo: reply?.toFfi(),
                    recentRows: ffiRows
                )
            }
        )
    }
}
