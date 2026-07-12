// The read-only NIP-29 host-browser projection (#108) -- pure functions,
// same shape as decodeNostrEntity (#116): no `NMPEngine` instance is
// needed to call any of these. Pass the returned `NMPDemand` straight to
// `NMPEngine.observe(NMPDemand)`. Mirrors NIP29.swift.

package com.nmp.sdk

import uniffi.nmp_ffi.FfiGroupRef
import uniffi.nmp_ffi.FfiRememberedGroups
import uniffi.nmp_ffi.FfiRow
import uniffi.nmp_ffi.activeAccountDemand as ffiActiveAccountDemand
import uniffi.nmp_ffi.decodeRememberedGroups as ffiDecodeRememberedGroups
import uniffi.nmp_ffi.groupContentDemand as ffiGroupContentDemand
import uniffi.nmp_ffi.groupDiscoveryDemand as ffiGroupDiscoveryDemand

/** A remembered NIP-29 group reference (#108, `FfiGroupRef` mirror) --
 * group id, host relay, and optional display name. */
data class GroupRef(
    val groupId: String,
    val host: String,
    val name: String?,
) {
    companion object {
        fun from(ffi: FfiGroupRef): GroupRef = GroupRef(ffi.groupId, ffi.host, ffi.name)
    }
}

/** The composed remembered-groups/host-relays value (#108,
 * `FfiRememberedGroups` mirror) -- what `decodeRememberedGroups` returns
 * from a delivered kind:10009 [Row]. */
data class RememberedGroups(
    val groups: List<GroupRef>,
    val hostsInUse: List<String>,
    val hasPrivateContent: Boolean,
) {
    companion object {
        fun from(ffi: FfiRememberedGroups): RememberedGroups =
            RememberedGroups(
                ffi.groups.map { GroupRef.from(it) },
                ffi.hostsInUse,
                ffi.hasPrivateContent,
            )
    }
}

/** The signed-in account's remembered-groups demand (#108): `kinds:
 * [10009]`, `AuthorOutboxes + Public`. Signed-out (no active account)
 * resolves to zero rows through the ordinary reactive-binding empty-
 * resolution path -- no special case needed on the caller's side. */
fun activeAccountDemand(): NMPDemand = NMPDemand.from(ffiActiveAccountDemand())

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

/** Decode a delivered kind:10009 [Row] into the composed remembered-
 * groups/host-relays value (#108). Infallible: malformed individual items
 * are dropped internally, never the whole decode. */
fun decodeRememberedGroups(row: Row): RememberedGroups {
    val ffiRow =
        FfiRow(
            id = row.id,
            pubkey = row.pubkey,
            createdAt = row.createdAt,
            kind = row.kind,
            tags = row.tags,
            content = row.content,
            sig = row.sig,
            sources = row.sources,
        )
    return RememberedGroups.from(ffiDecodeRememberedGroups(ffiRow))
}
