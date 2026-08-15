// NIP-51 Simple groups, exposed with the NIP-29 product capability:
// tolerant, observational parsing (#863/#1551).
// Mirrors NIP29SimpleGroups.swift.
//
// A [Row] handed to this file may have been constructed by the app itself --
// any kind, any signature, no sources. So the parser is deliberately
// tolerant and its result is deliberately plain data. There is no
// observation-qualified wrapper, projection-error family, or frame proof
// here, and `scripts/check-nip29-surfaces.sh` fails the build if
// one is reintroduced.
//
// Reading kind:10009 stays the ordinary demand/observation noun
// (`currentAccountGroupListDemand()`, below). Typed add/remove methods compile
// through Rust's durable semantic operation and return the ordinary [Receipt].
// Browsing a NIP-29 group still takes an explicit, caller-supplied relay set --
// see `NMPRelayScope.on`.
//
// [SimpleGroupsList] is also the ONE native shape a decoded kind:10009 list
// takes (#858). The NIP-29-facing wrapper family that used to sit beside it
// merely renamed these fields and dropped `malformedItemCount`; there is no
// second projection of this value anywhere in the SDK.

package com.nmp.sdk

import uniffi.nmp_ffi.FfiRow
import uniffi.nmp_ffi.FfiGroupListActionException
import uniffi.nmp_ffi.FfiSimpleGroupEntry
import uniffi.nmp_ffi.FfiSimpleGroupsList
import uniffi.nmp_ffi.currentAccountGroupListDemand as ffiCurrentAccountGroupListDemand
import uniffi.nmp_ffi.parseSimpleGroupsListTolerant as ffiParseSimpleGroupsListTolerant

/** One tolerantly parsed Simple-groups entry -- group id, host relay,
 * optional display name.
 *
 * [hostRelay] is a canonically spelled *observed* string. It is not a
 * routing permission: passing it to `NMPRelayScope.on` is the app's own
 * explicit decision, and that constructor parses it like any other
 * caller-supplied host. */
data class SimpleGroupEntry(
    val groupId: String,
    val hostRelay: String,
    val name: String?,
) {
    companion object {
        fun from(ffi: FfiSimpleGroupEntry): SimpleGroupEntry =
            SimpleGroupEntry(ffi.groupId, ffi.hostRelay, ffi.name)
    }
}

/** Tolerantly parsed Simple groups data. Item and relay ordering is
 * preserved. Malformed public items and encrypted private content remain
 * explicit evidence rather than disappearing at the native boundary.
 *
 * This value is **observational only**. It may have been produced from a
 * wholly caller-constructed [Row] of any kind, and it grants no signature,
 * canonical-store, provenance, routing, or mutation authority. */
data class SimpleGroupsList(
    val items: List<SimpleGroupEntry>,
    val relaysInUse: List<String>,
    val malformedItemCount: ULong,
    val hasPrivateContent: Boolean,
) {
    companion object {
        fun from(ffi: FfiSimpleGroupsList): SimpleGroupsList =
            SimpleGroupsList(
                ffi.items.map { SimpleGroupEntry.from(it) },
                ffi.relaysInUse,
                ffi.malformedItemCount,
                ffi.hasPrivateContent,
            )
    }
}

/** A typed group-list action was refused before ordinary receipt custody. */
sealed class GroupListActionError(message: String) : Exception(message) {
    data class InvalidRelayUrl(val got: String) :
        GroupListActionError("invalid relay URL: $got")

    data object AutomaticRoutingUnavailable :
        GroupListActionError("automatic author/outbox routing is not configured")

    data object SignedOut : GroupListActionError("no current account is selected")
    data object EngineClosed : GroupListActionError("the engine is closed")
    data object ReceiptUnavailable :
        GroupListActionError("the group-list operation was refused before receipt custody")

    companion object {
        internal fun from(error: FfiGroupListActionException): GroupListActionError =
            when (error) {
                is FfiGroupListActionException.InvalidRelayUrl -> InvalidRelayUrl(error.got)
                is FfiGroupListActionException.AutomaticRoutingUnavailable ->
                    AutomaticRoutingUnavailable
                is FfiGroupListActionException.SignedOut -> SignedOut
                is FfiGroupListActionException.EngineClosed -> EngineClosed
                is FfiGroupListActionException.ReceiptUnavailable -> ReceiptUnavailable
            }
    }
}

/** The signed-in account's Simple-groups-list demand (#108): `kinds:
 * [10009]`, `AuthorOutboxes + Public`. Signed-out (no current account)
 * resolves to zero rows through the ordinary reactive-binding empty-
 * resolution path -- no special case needed on the caller's side.
 *
 * #1551 places this NIP-51-defined list with the NIP-29 product capability
 * that consumes it, without changing which NIP defines kind:10009. */
fun currentAccountGroupListDemand(): NMPDemand = NMPDemand.from(ffiCurrentAccountGroupListDemand())

/** Tolerantly parse Simple-groups-shaped public items from an untrusted
 * [row] (#863). Infallible and kind-agnostic: malformed individual items are
 * counted, never fatal, and the row's `kind`/signature are not consulted.
 *
 * The result is observational data only. */
fun parseSimpleGroupsListTolerant(row: Row): SimpleGroupsList {
    val ffiRow =
        FfiRow(
            id = row.id,
            pubkey = row.pubkey,
            createdAt = row.createdAt,
            kind = row.kind,
            tags = row.tags,
            content = row.content,
            signature = row.signature.toFfi(),
            sources = row.sources,
        )
    return SimpleGroupsList.from(ffiParseSimpleGroupsListTolerant(ffiRow))
}

/** Add one exact `(group id, canonical host)` identity through the ordinary
 * durable receipt. The host carried by the list is not a publish route. */
fun NMPEngine.addGroupToList(
    groupId: String,
    hostRelay: String,
    name: String? = null,
): Receipt =
    groupListReceipt { ffi.addGroupToList(groupId, hostRelay, name) }

/** Remove every valid public group tag with this exact identity. */
fun NMPEngine.removeGroupFromList(
    groupId: String,
    hostRelay: String,
): Receipt =
    groupListReceipt { ffi.removeGroupFromList(groupId, hostRelay) }

/** Add one canonical relay-in-use tag without changing group tags. */
fun NMPEngine.addRelayInUse(relay: String): Receipt =
    groupListReceipt { ffi.addRelayInUse(relay) }

/** Remove every valid equivalent relay-in-use tag without changing group
 * tags or malformed evidence. */
fun NMPEngine.removeRelayInUse(relay: String): Receipt =
    groupListReceipt { ffi.removeRelayInUse(relay) }

private fun NMPEngine.groupListReceipt(action: () -> uniffi.nmp_ffi.NmpReceiptStream): Receipt =
    try {
        receiptFrom(action())
    } catch (error: FfiGroupListActionException) {
        throw GroupListActionError.from(error)
    }
