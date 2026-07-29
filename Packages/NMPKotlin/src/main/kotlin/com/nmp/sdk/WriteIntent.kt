// The write noun, in ergonomic Kotlin shape. Mirrors WriteIntent.swift.

package com.nmp.sdk

import uniffi.nmp_ffi.FfiDurability
import uniffi.nmp_ffi.FfiEventBuilder
import uniffi.nmp_ffi.FfiIdentity
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

    companion object {
        internal fun from(ffi: FfiDurability): Durability =
            when (ffi) {
                FfiDurability.DURABLE -> Durable
                FfiDurability.EPHEMERAL -> Ephemeral
                FfiDurability.AT_MOST_ONCE -> AtMostOnce
            }
    }
}

/** Where a write is routed. The whole vocabulary is two words: [Auto]
 * ("figure out how to route whatever I'm publishing") and [Explicit] ("use
 * these exact relays and that is that"). There is no third word -- no
 * "outbox", no NIP name, no strategy label -- because which strategy claims
 * a kind is NMP's own business, decided at send time.
 *
 * [Explicit] is a general capability, not a protocol-module privilege: an
 * app offering "publish this event to relay: [user input]", a wiki module
 * publishing to the user's preferred wiki relays, and a user archiving
 * someone else's signed note to their own relay are all the same primitive.
 * It executes verbatim -- the relay directory is never consulted, and
 * nothing added to it later widens it -- and an empty [relays] is refused
 * at the door with `NMPError`, never quietly downgraded to [Auto]. */
sealed class WriteRouting {
    object Auto : WriteRouting()

    data class Explicit(val relays: List<String>) : WriteRouting()

    fun toFfi(): FfiWriteRouting =
        when (this) {
            is Auto -> FfiWriteRouting.Auto
            is Explicit -> FfiWriteRouting.Explicit(relays)
        }

    companion object {
        internal fun from(ffi: FfiWriteRouting): WriteRouting =
            when (ffi) {
                is FfiWriteRouting.Auto -> Auto
                is FfiWriteRouting.Explicit -> Explicit(ffi.relays)
            }
    }
}

/** The event payload of a write intent (`FfiWritePayload` mirror). VISION
 * P: signing and publishing are ORTHOGONAL stages -- [Event] describes an
 * event NMP stamps, freezes and signs itself. The kind is the one thing it
 * cannot invent, so the kind is the one thing it demands; the account it
 * publishes as comes from the write's identity (see
 * [NMPEngine.setActiveAccount] and [WriteIntent.identity]), never
 * from the payload, and [Event.createdAt] is stamped at acceptance unless
 * you state one -- state one and it is kept exactly.
 *
 * [Signed] (#32, the M5 unlock) is a caller that already holds a
 * validly-signed event -- an external signer provider, or a verbatim
 * republish of somebody else's note to an archive relay -- and hands its
 * fields across as-is: the engine verifies then publishes it exactly as
 * given, never re-signing, mutating a tag, or recomputing an id. */
sealed class WritePayload {
    /** Everything you must say is [kind]. [tags], [content] and
     * [createdAt] default, and there is deliberately no `pubkey`, `id` or
     * `sig`. */
    data class Event(
        val kind: UShort,
        val tags: List<List<String>> = emptyList(),
        val content: String = "",
        val createdAt: ULong? = null,
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
            is Event -> FfiWritePayload.Event(FfiEventBuilder(kind, tags, content, createdAt))
            is Signed -> FfiWritePayload.Signed(id, pubkey, createdAt, kind, tags, content, sig)
        }

    companion object {
        internal fun from(ffi: FfiWritePayload): WritePayload =
            when (ffi) {
                is FfiWritePayload.Event ->
                    Event(
                        ffi.builder.kind,
                        ffi.builder.tags,
                        ffi.builder.content,
                        ffi.builder.createdAt,
                    )
                is FfiWritePayload.Signed ->
                    Signed(
                        ffi.id,
                        ffi.pubkey,
                        ffi.createdAt,
                        ffi.kind,
                        ffi.tags,
                        ffi.content,
                        ffi.sig,
                    )
            }
    }
}

/** The identity one write publishes under (`FfiIdentity` mirror). Exactly
 * two words, and neither of them is an absence: [Active] is a positive
 * instruction ("whoever is the active account when this is accepted"),
 * which is why there is no third "unset" state here or anywhere else.
 *
 * On a [WritePayload.Event] payload the identity SELECTS the author -- a
 * builder states none, so there is nothing for it to contradict. On a
 * [WritePayload.Signed] payload it may only RESTATE the author already
 * frozen in the bytes: naming that author changes nothing, naming anybody
 * else is a consent/author contradiction that surfaces as
 * [WriteStatus.Failed] on the receipt stream with no [WriteStatus.Accepted]
 * before it.
 *
 * [Explicit.pubkey] is 64-char HEX and nothing else. A bech32 `npub` is
 * refused however well-formed it is ([NMPError.InvalidPublicKey], thrown
 * synchronously from `publish`): bech32 is how something is shown to a
 * person or received from one, so an app that took an npub from a paste box
 * decodes it there -- with `decodeNostrEntity` -- and hands NMP a key.
 * Naming a pubkey with no registered signer is NOT an error: the write
 * parks as [WriteStatus.AwaitingCapability] until that capability attaches.
 * Acceptance pins the resolved key either way, so a later
 * [NMPEngine.setActiveAccount] cannot retarget the write. */
sealed class Identity {
    object Active : Identity()

    data class Explicit(val pubkey: String) : Identity()

    fun toFfi(): FfiIdentity =
        when (this) {
            is Active -> FfiIdentity.Active
            is Explicit -> FfiIdentity.Explicit(pubkey)
        }

    companion object {
        internal fun from(ffi: FfiIdentity): Identity =
            when (ffi) {
                is FfiIdentity.Active -> Active
                is FfiIdentity.Explicit -> Explicit(ffi.pubkey)
            }
    }
}

/** A caller's publish request (`FfiWriteIntent` mirror).
 *
 * [identity] (#47) defaults to [Identity.Active] -- the overwhelming
 * majority of writes publish as the logged-in account, and saying so costs
 * nothing. [WriteStatus.AwaitingCapability.pubkey] (#47 Unit B) is the
 * exact frozen identity parked -- the key [Identity.Explicit] named, else
 * the account active at publish time -- never the (possibly different)
 * currently active account. */
data class WriteIntent(
    val payload: WritePayload,
    val durability: Durability,
    val routing: WriteRouting,
    val identity: Identity = Identity.Active,
    /** Crash-safe client correlation token (#591). `null` -- the default --
     * opts this write out of correlation entirely. A non-`null` token is
     * validated (non-empty, length-capped) on the way across the boundary;
     * a malformed token throws `NMPError.InvalidCorrelationToken`
     * synchronously from `publish`, before any engine call. A token that
     * already resolves to a previously-accepted receipt reattaches that
     * existing obligation instead of enqueuing a second write -- no body
     * comparison against [payload]. See [NMPEngine.reattachReceipt] (the
     * correlation overload) for the door that recovers a receipt after a
     * crash that happened BEFORE the app could durably persist the id
     * `publish` returned. */
    val correlation: String? = null,
) {
    fun toFfi(): FfiWriteIntent =
        FfiWriteIntent(
            payload = payload.toFfi(),
            durability = durability.toFfi(),
            routing = routing.toFfi(),
            identity = identity.toFfi(),
            correlation = correlation,
        )

    companion object {
        /** Reverse projection for protocol-owned FFI composers that return the
         * ordinary write noun. Internal so apps receive a [WriteIntent] rather
         * than raw generated FFI vocabulary. */
        internal fun from(ffi: FfiWriteIntent): WriteIntent =
            WriteIntent(
                payload = WritePayload.from(ffi.payload),
                durability = Durability.from(ffi.durability),
                routing = WriteRouting.from(ffi.routing),
                identity = Identity.from(ffi.identity),
                correlation = ffi.correlation,
            )
    }
}

/** Every state a publish's receipt stream may report (ledger #9: enqueue is
 * not converged -- many of these may arrive per publish, one per relay for
 * the terminal states). */
sealed class WriteStatus {
    object Accepted : WriteStatus()

    object Cancelled : WriteStatus()

    object Superseded : WriteStatus()

    /** #47 Unit B: [pubkey] is the exact frozen identity (64-char hex) no
     * registered signer currently answers for. Retained, not terminal --
     * re-arrives verbatim on restart replay and resumes only when a signer
     * for THIS pubkey attaches, never a different one. */
    data class AwaitingCapability(val pubkey: String) : WriteStatus()

    data class Signed(val eventId: String) : WriteStatus()

    data class Routed(val relays: List<String>) : WriteStatus()

    data class AwaitingRelay(val relay: String) : WriteStatus()

    data class AwaitingAuth(val relay: String) : WriteStatus()

    data class RetryEligible(val relay: String, val attempt: ULong, val eligibleAt: ULong) : WriteStatus()

    data class HandoffAmbiguous(val relay: String, val attempt: ULong, val observedAt: ULong) : WriteStatus()

    data class Sent(val relay: String, val attempt: ULong, val writtenAt: ULong) : WriteStatus()

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
                is FfiWriteStatus.Cancelled -> Cancelled
                is FfiWriteStatus.Superseded -> Superseded
                is FfiWriteStatus.AwaitingCapability -> AwaitingCapability(ffi.pubkey)
                is FfiWriteStatus.Signed -> Signed(ffi.eventId)
                is FfiWriteStatus.Routed -> Routed(ffi.relays)
                is FfiWriteStatus.AwaitingRelay -> AwaitingRelay(ffi.relay)
                is FfiWriteStatus.AwaitingAuth -> AwaitingAuth(ffi.relay)
                is FfiWriteStatus.RetryEligible ->
                    RetryEligible(ffi.relay, ffi.attempt, ffi.eligibleAt)
                is FfiWriteStatus.HandoffAmbiguous ->
                    HandoffAmbiguous(ffi.relay, ffi.attempt, ffi.observedAt)
                is FfiWriteStatus.Sent -> Sent(ffi.relay, ffi.attempt, ffi.writtenAt)
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
