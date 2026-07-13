// The write noun, in ergonomic Kotlin shape. Mirrors WriteIntent.swift.

package com.nmp.sdk

import uniffi.nmp_ffi.FfiDurability
import uniffi.nmp_ffi.FfiWriteIntent
import uniffi.nmp_ffi.FfiWritePayload
import uniffi.nmp_ffi.FfiWriteRouting
import uniffi.nmp_ffi.FfiWriteStatus

/** A durability PROPERTY of a write (not a routing choice). */
enum class Durability {
    Durable,
    Ephemeral,
    AtMostOnce,
    ;

    fun toFfi(): FfiDurability =
        when (this) {
            Durable -> FfiDurability.DURABLE
            Ephemeral -> FfiDurability.EPHEMERAL
            AtMostOnce -> FfiDurability.AT_MOST_ONCE
        }
}

/** Where a write is routed. There is deliberately no `PrivateNarrow` case
 * (#22/#52): a private/narrow route must come from a trusted protocol
 * module's own resolved logic, never a raw relay-URL string an app hands
 * across this boundary with no way to prove it is actually private --
 * exactly the "route escape hatch" #22's canonical design rules out. See
 * `FfiWriteRouting`'s doc. */
sealed class WriteRouting {
    object AuthorOutbox : WriteRouting()

    data class ToInboxes(val recipients: List<String>) : WriteRouting()

    fun toFfi(): FfiWriteRouting =
        when (this) {
            is AuthorOutbox -> FfiWriteRouting.AuthorOutbox
            is ToInboxes -> FfiWriteRouting.ToInboxes(recipients)
        }
}

/** The event payload of a write intent (`FfiWritePayload` mirror). VISION
 * P: signing and publishing are ORTHOGONAL stages -- `Unsigned` is a
 * template whose `pubkey` names the account being published as (see
 * `NMPEngine.setActiveAccount`); the key lives engine-side and signs it
 * there. `Signed` (#32, the M5 unlock) is a caller that already holds a
 * validly-signed event -- an external signer / NIP-46 bunker, or a
 * verbatim republish -- and hands its fields across as-is: the engine
 * verifies then publishes it exactly as given, never re-signing, mutating
 * a tag, or recomputing an id. */
sealed class WritePayload {
    data class Unsigned(
        val pubkey: String,
        val createdAt: ULong,
        val kind: UShort,
        val tags: List<List<String>>,
        val content: String,
    ) : WritePayload()

    data class Signed(
        val id: String,
        val pubkey: String,
        val createdAt: ULong,
        val kind: UShort,
        val tags: List<List<String>>,
        val content: String,
        val sig: String,
    ) : WritePayload()

    fun toFfi(): FfiWritePayload =
        when (this) {
            is Unsigned -> FfiWritePayload.Unsigned(pubkey, createdAt, kind, tags, content)
            is Signed -> FfiWritePayload.Signed(id, pubkey, createdAt, kind, tags, content, sig)
        }
}

/** A caller's publish request (`FfiWriteIntent` mirror). */
data class WriteIntent(
    val payload: WritePayload,
    val durability: Durability,
    val routing: WriteRouting,
) {
    fun toFfi(): FfiWriteIntent =
        FfiWriteIntent(
            payload = payload.toFfi(),
            durability = durability.toFfi(),
            routing = routing.toFfi(),
        )
}

/** Every state a publish's receipt stream may report (ledger #9: enqueue is
 * not converged -- many of these may arrive per publish, one per relay for
 * the terminal states). */
sealed class WriteStatus {
    object Accepted : WriteStatus()

    object AwaitingCapability : WriteStatus()

    data class Signed(val eventId: String) : WriteStatus()

    data class Routed(val relays: List<String>) : WriteStatus()

    data class Sent(val relay: String) : WriteStatus()

    data class Acked(val relay: String) : WriteStatus()

    data class Rejected(val relay: String, val reason: String) : WriteStatus()

    data class GaveUp(val relay: String) : WriteStatus()

    data class PersistenceBlocked(val relay: String) : WriteStatus()

    data class RoutePersistenceBlocked(val relay: String) : WriteStatus()

    data class OutcomeUnknown(val relay: String) : WriteStatus()

    data class ReplaceableConflict(val expected: String?, val actual: String?) : WriteStatus()

    data class Failed(val reason: String) : WriteStatus()

    companion object {
        fun from(ffi: FfiWriteStatus): WriteStatus =
            when (ffi) {
                is FfiWriteStatus.Accepted -> Accepted
                is FfiWriteStatus.AwaitingCapability -> AwaitingCapability
                is FfiWriteStatus.Signed -> Signed(ffi.eventId)
                is FfiWriteStatus.Routed -> Routed(ffi.relays)
                is FfiWriteStatus.Sent -> Sent(ffi.relay)
                is FfiWriteStatus.Acked -> Acked(ffi.relay)
                is FfiWriteStatus.Rejected -> Rejected(ffi.relay, ffi.reason)
                is FfiWriteStatus.GaveUp -> GaveUp(ffi.relay)
                is FfiWriteStatus.PersistenceBlocked -> PersistenceBlocked(ffi.relay)
                is FfiWriteStatus.RoutePersistenceBlocked -> RoutePersistenceBlocked(ffi.relay)
                is FfiWriteStatus.OutcomeUnknown -> OutcomeUnknown(ffi.relay)
                is FfiWriteStatus.ReplaceableConflict ->
                    ReplaceableConflict(ffi.expected, ffi.actual)
                is FfiWriteStatus.Failed -> Failed(ffi.reason)
            }
    }
}
