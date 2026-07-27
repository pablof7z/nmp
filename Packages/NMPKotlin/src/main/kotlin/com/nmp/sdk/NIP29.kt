// The read-only NIP-29 host-browser projection (#108) -- pure functions,
// same shape as decodeNostrEntity (#116): no `NMPEngine` instance is
// needed to call any of these. Pass the returned `NMPDemand` straight to
// `NMPEngine.observe(NMPDemand)`. Mirrors NIP29.swift.
//
// `NMPEngine.groupMessageIntent`/`GroupSendIntent` (#156) are this file's
// write-side counterpart. The app supplies semantic composer state; NMP owns
// author/time/kind, NIP-27 mention materialization, `p`/reply-`e` tags, and
// `h`/pinned-host composition.
//
// #858: nothing here re-labels NIP-51's value. A kind:10009 Simple-groups
// list is decoded once, as itself, by `parseSimpleGroupsListTolerant` in
// NIP51.kt; the app selects one `SimpleGroupEntry` and passes its exact
// `hostRelay`/`groupId` to the constructors below. This file declares no
// NIP-51 record type and no decode function of its own.

package com.nmp.sdk

import uniffi.nmp_ffi.FfiComposedWriteIntent
import uniffi.nmp_ffi.FfiGroupReplyParent
import uniffi.nmp_ffi.NmpEngineInterface
import uniffi.nmp_ffi.groupContentDemand as ffiGroupContentDemand
import uniffi.nmp_ffi.groupDiscoveryDemand as ffiGroupDiscoveryDemand

/** Group discovery (kind:39000) pinned to [host] (#108). Throws
 * `NMPError.InvalidRelayUrl` if `host` doesn't parse. */
fun groupDiscoveryDemand(host: String): NMPDemand =
    NMPDemand.from(nmpRethrowing { ffiGroupDiscoveryDemand(host) })

/** Group content (kinds 9, 30315), `h`-tag scoped to [groupId], pinned to
 * [host] (#108). Throws `NMPError.InvalidRelayUrl` if `host` doesn't
 * parse. */
fun groupContentDemand(
    host: String,
    groupId: String,
): NMPDemand = NMPDemand.from(nmpRethrowing { ffiGroupContentDemand(host, groupId) })

/** A direct reply parent for a kind:9 group message. NMP turns this into the
 * marked reply `e` row plus the author's deduplicated recipient `p` row. */
data class GroupReplyParent(
    val eventId: String,
    val authorPubkey: String,
) {
    internal fun toFfi(): FfiGroupReplyParent = FfiGroupReplyParent(eventId, authorPubkey)
}

/** A composed NIP-29 group message (#156), returned by
 * [NMPEngine.groupMessageIntent].
 * Opaque and take-once -- pass it to `NMPEngine.publishComposed` exactly
 * once; a second attempt throws `NMPError.IntentAlreadyConsumed`. Never
 * exposes the materialized tags, routing, author, or timestamp. */
class GroupSendIntent internal constructor(internal val ffi: FfiComposedWriteIntent)

/** Internal bridge used by [NMPEngine.groupMessageIntent]. Native callers
 * supply no author, time, kind, or raw tags. */
internal fun composeGroupMessageIntent(
    engine: NmpEngineInterface,
    host: String,
    groupId: String,
    content: String,
    recipients: List<String>,
    reply: GroupReplyParent?,
): GroupSendIntent {
    return GroupSendIntent(
        nmpRethrowing {
            engine.groupMessageIntent(
                host,
                groupId,
                content,
                recipients,
                reply?.toFfi(),
            )
        },
    )
}
