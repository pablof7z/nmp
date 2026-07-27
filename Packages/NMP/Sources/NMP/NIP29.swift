// The read-only NIP-29 host-browser projection (#108) -- pure functions,
// same shape as `decodeNostrEntity` (#116): no `NMPEngine` instance is
// needed to call any of these. Pass the returned `NMPDemand` straight to
// `NMPEngine.observe(_ demand:)`, exactly like any other `NMPDemand`.
//
// `NMPEngine.groupMessageIntent`/`GroupSendIntent` (#156) are this file's
// write-side counterpart. The app supplies semantic composer state; NMP owns
// author/time/kind, NIP-27 mention materialization, `p`/reply-`e` tags, and
// `h`/pinned-host composition.
//
// #858: nothing here re-labels NIP-51's value. A kind:10009 Simple-groups
// list is decoded once, as itself, by `parseSimpleGroupsListTolerant(_:)` in
// NIP51.swift; the app selects one `SimpleGroupEntry` and passes its exact
// `hostRelay`/`groupId` to the constructors below. This file declares no
// NIP-51 record type and no decode function of its own.

import NMPFFI

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
    /// direct-parent `e` row and its author recipient. NMP temporarily omits
    /// NIP-29 `previous` until it can prove a live host acceptance window;
    /// native code cannot supply or forge relay provenance.
    ///
    /// The active account supplies the author and NMP supplies event time.
    /// The caller cannot choose a kind or inject raw tags. Publish the opaque
    /// result via `publishComposed(_:)`.
    public func groupMessageIntent(
        host: String,
        groupID: String,
        content: String,
        recipients: [String] = [],
        reply: GroupReplyParent? = nil
    ) throws -> GroupSendIntent {
        return try GroupSendIntent(
            ffi: nmpRethrowing {
                try ffi.groupMessageIntent(
                    host: host,
                    groupId: groupID,
                    content: content,
                    recipientPubkeys: recipients,
                    replyTo: reply?.toFfi()
                )
            }
        )
    }
}
