// NIP-51 Simple groups: tolerant, observational parsing (#863).
// Mirrors NIP51.swift.
//
// A [Row] handed to this file may have been constructed by the app itself --
// any kind, any signature, no sources. So the parser is deliberately
// tolerant and its result is deliberately plain data. There is no
// observation-qualified wrapper, projection-error family, or frame proof
// here, and `scripts/check-nip51-no-derived-authority.sh` fails the build if
// one is reintroduced.
//
// Reading kind:10009 stays the ordinary demand/observation noun
// (`activeAccountDemand()`, below). Browsing a NIP-29 group still takes an
// explicit, caller-supplied relay set -- see `NMPRelayScope.on`.
//
// [SimpleGroupsList] is also the ONE native shape a decoded kind:10009 list
// takes (#858). The NIP-29-facing wrapper family that used to sit beside it
// merely renamed these fields and dropped `malformedItemCount`; there is no
// second projection of this value anywhere in the SDK.

package com.nmp.sdk

import uniffi.nmp_ffi.FfiRow
import uniffi.nmp_ffi.FfiSimpleGroupEntry
import uniffi.nmp_ffi.FfiSimpleGroupsList
import uniffi.nmp_ffi.activeAccountDemand as ffiActiveAccountDemand
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

/** The signed-in account's Simple-groups-list demand (#108): `kinds:
 * [10009]`, `AuthorOutboxes + Public`. Signed-out (no active account)
 * resolves to zero rows through the ordinary reactive-binding empty-
 * resolution path -- no special case needed on the caller's side.
 *
 * #858 moved this out of NIP29.kt: kind:10009 is NIP-51's kind, so its
 * demand constructor lives with the rest of NIP-51. */
fun activeAccountDemand(): NMPDemand = NMPDemand.from(ffiActiveAccountDemand())

/** Tolerantly parse Simple-groups-shaped public items from an untrusted
 * [row] (#863). Infallible and kind-agnostic: malformed individual items are
 * counted, never fatal, and the row's `kind`/`sig` are not consulted.
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
            sig = row.sig,
            signatureState = row.signatureState.toFfi(),
            sources = row.sources,
        )
    return SimpleGroupsList.from(ffiParseSimpleGroupsListTolerant(ffiRow))
}
