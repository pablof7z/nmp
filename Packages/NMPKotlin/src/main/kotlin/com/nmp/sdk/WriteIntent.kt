// The write noun, in ergonomic Kotlin shape. Mirrors WriteIntent.swift.

package com.nmp.sdk

import uniffi.nmp_ffi.FfiAuthDenialSource
import uniffi.nmp_ffi.FfiEventBuilder
import uniffi.nmp_ffi.FfiIdentity
import uniffi.nmp_ffi.FfiRetryCause
import uniffi.nmp_ffi.FfiWriteIntent
import uniffi.nmp_ffi.FfiWritePayload
import uniffi.nmp_ffi.FfiWriteRouting
import uniffi.nmp_ffi.FfiNotSentReason
import uniffi.nmp_ffi.FfiPublishQueueEntry
import uniffi.nmp_ffi.FfiRefuseReason
import uniffi.nmp_ffi.FfiRelayState
import uniffi.nmp_ffi.FfiRelayWaiting
import uniffi.nmp_ffi.FfiSigningState
import uniffi.nmp_ffi.FfiWriteFact
import uniffi.nmp_ffi.FfiWriteOutcome

enum class AuthDenialSource {
    Policy,
    Signer,
    Relay,
    ;

    companion object {
        internal fun from(ffi: FfiAuthDenialSource): AuthDenialSource =
            when (ffi) {
                FfiAuthDenialSource.POLICY -> Policy
                FfiAuthDenialSource.SIGNER -> Signer
                FfiAuthDenialSource.RELAY -> Relay
            }
    }
}

enum class RetryCause {
    Interrupted,
    AckTimeout,
    ConnectionLost,
    RelayRateLimited,
    RelayError,
    ;

    companion object {
        internal fun from(ffi: FfiRetryCause): RetryCause =
            when (ffi) {
                FfiRetryCause.INTERRUPTED -> Interrupted
                FfiRetryCause.ACK_TIMEOUT -> AckTimeout
                FfiRetryCause.CONNECTION_LOST -> ConnectionLost
                FfiRetryCause.RELAY_RATE_LIMITED -> RelayRateLimited
                FfiRetryCause.RELAY_ERROR -> RelayError
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
 * `NMPError.PublishRefused` from `publish` itself -- an instruction that
 * cannot resolve is a refusal, not a parked hope
 * before it.
 *
 * [Explicit.pubkey] is 64-char HEX and nothing else. A bech32 `npub` is
 * refused however well-formed it is ([NMPError.InvalidPublicKey], thrown
 * synchronously from `publish`): bech32 is how something is shown to a
 * person or received from one, so an app that took an npub from a paste box
 * decodes it there -- with `decodeNostrEntity` -- and hands NMP a key.
 * Naming a pubkey with no registered signer is NOT an error: the write
 * parks as [SigningState.AwaitingSigner] until that capability attaches.
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
 * nothing. [SigningState.AwaitingSigner.pubkey] (#47 Unit B) is the
 * exact frozen identity parked -- the key [Identity.Explicit] named, else
 * the account active at publish time -- never the (possibly different)
 * currently active account. */
data class WriteIntent(
    val payload: WritePayload,
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
                routing = WriteRouting.from(ffi.routing),
                identity = Identity.from(ffi.identity),
                correlation = ffi.correlation,
            )
    }
}

/** The signing state of the WHOLE write -- one signature, one author, one
 * answer. */
sealed class SigningState {
    /** No registered signer answers for [pubkey] (64-char hex) -- the exact
     * identity FROZEN at acceptance, never whoever is active now. Re-armed
     * only by attaching a signer for THIS key, and re-emitted verbatim on
     * restart replay.
     *
     * **No clock ever ends this.** A device whose signer is simply not
     * plugged in yet is not a device whose write failed; the app's own
     * decision is the only other exit, and it is two calls: cancel the
     * write, then remove the terminal queue entry it leaves behind.
     *
     * This is the state a person has to be told about, and [InFlight] is the
     * one it must never be confused with. */
    data class AwaitingSigner(val pubkey: String) : SigningState()

    /** A signer for [pubkey] (64-char hex) HAS the request and has not
     * answered yet -- the ordinary state of every healthy write between
     * acceptance and signature promotion.
     *
     * Transient and normal: it ends when the signer answers ([Signed] or
     * [Refused]), or falls back to [AwaitingSigner] if that signer becomes
     * unavailable. Nothing here is a reason to trouble a user. */
    data class InFlight(val pubkey: String) : SigningState()

    data class Signed(val eventId: String) : SigningState()

    /** The signer answered and said no. Terminal for the whole write. */
    data class Refused(val reason: String) : SigningState()

    companion object {
        internal fun from(ffi: FfiSigningState): SigningState =
            when (ffi) {
                is FfiSigningState.AwaitingSigner -> AwaitingSigner(ffi.pubkey)
                is FfiSigningState.InFlight -> InFlight(ffi.pubkey)
                is FfiSigningState.Signed -> Signed(ffi.eventId)
                is FfiSigningState.Refused -> Refused(ffi.reason)
            }
    }
}

/** Why a relay lane is not attempting right now. Every case is a fact about
 * the lane; none of them is a deadline. */
sealed class RelayWaiting {
    /** Offline time consumes no attempt ordinal, so being offline can never
     * spend the give-up ceiling. */
    object NotConnected : RelayWaiting()

    object NeedsAuth : RelayWaiting()

    /** The last attempt failed in a way that permits another one, and
     * [cause]/[detail] say WHY -- "we will try again" and "we will try again
     * because the relay rate-limited us" are different messages and only the
     * second one can be acted on. */
    data class BackingOff(
        val attempt: ULong,
        val eligibleAt: ULong,
        val cause: RetryCause,
        val detail: String?,
    ) : RelayWaiting()

    /** The lane is owned and nonterminal, but a durable fact about it could
     * not be committed -- the local disk is refusing writes. No wire EVENT
     * was emitted. Also latched onto the queue entry and never cleared by a
     * later ack. */
    data class PersistenceStalled(val detail: String) : RelayWaiting()

    companion object {
        internal fun from(ffi: FfiRelayWaiting): RelayWaiting =
            when (ffi) {
                is FfiRelayWaiting.NotConnected -> NotConnected
                is FfiRelayWaiting.NeedsAuth -> NeedsAuth
                is FfiRelayWaiting.BackingOff ->
                    BackingOff(
                        ffi.attempt,
                        ffi.eligibleAt,
                        RetryCause.from(ffi.cause),
                        ffi.detail,
                    )
                is FfiRelayWaiting.PersistenceStalled -> PersistenceStalled(ffi.detail)
            }
    }
}

/** What is true at ONE relay. [Published], [Rejected], [AuthFailed] and
 * [GaveUp] are terminal for that relay; [Waiting] and [Sent] are not. */
sealed class RelayState {
    data class Waiting(val waiting: RelayWaiting) : RelayState()

    /** Transport proved socket write + flush. Not an ack, and not terminal. */
    data class Sent(val attempt: ULong, val writtenAt: ULong) : RelayState()

    object Published : RelayState()

    /** The relay authenticated the identity and refused THIS EVENT. The
     * repair is to the event. */
    data class Rejected(val reason: String) : RelayState()

    /** The write could not be authenticated HERE. Deliberately NOT folded
     * into [Rejected]: [source] keeps an app's own decision not to
     * authenticate from being shown to a user as a relay refusing them. */
    data class AuthFailed(
        val pubkey: String,
        val source: AuthDenialSource,
        val reason: String,
    ) : RelayState()

    /** The attempt ceiling was reached at this relay. Terminal HERE and
     * nowhere else: three relays published and one given up on is a success
     * with a footnote, not a failed write. */
    object GaveUp : RelayState()

    /** Whether this relay will produce another fact. */
    val isTerminal: Boolean
        get() =
            when (this) {
                is Published, is Rejected, is AuthFailed, is GaveUp -> true
                is Waiting, is Sent -> false
            }

    companion object {
        internal fun from(ffi: FfiRelayState): RelayState =
            when (ffi) {
                is FfiRelayState.Waiting -> Waiting(RelayWaiting.from(ffi.waiting))
                is FfiRelayState.Sent -> Sent(ffi.attempt, ffi.writtenAt)
                is FfiRelayState.Published -> Published
                is FfiRelayState.Rejected -> Rejected(ffi.reason)
                is FfiRelayState.AuthFailed ->
                    AuthFailed(
                        ffi.pubkey,
                        AuthDenialSource.from(ffi.source),
                        ffi.reason,
                    )
                is FfiRelayState.GaveUp -> GaveUp
            }
    }
}

/** Why a write ended without going anywhere. */
enum class NotSentReason {
    Cancelled,

    /** A newer accepted write won the same replaceable coordinate before this
     * one started any wire attempt. Not a failure -- for an app renewing
     * presence it is the steady state. */
    Superseded,
    ;

    companion object {
        internal fun from(ffi: FfiNotSentReason): NotSentReason =
            when (ffi) {
                FfiNotSentReason.CANCELLED -> Cancelled
                FfiNotSentReason.SUPERSEDED -> Superseded
            }
    }
}

/** Why the acceptance door said no. */
sealed class RefuseReason {
    object AlreadyExpired : RefuseReason()

    object Tombstoned : RefuseReason()

    object ReplaceableBaseOnRegularEvent : RefuseReason()

    /** A whole-value replacement lost its compare-and-swap.
     *
     * BOTH ids are kept, and that is what makes the failure recoverable
     * without the user: fetch [actual], reapply the change and resubmit
     * silently. Reduced to a string you could only tell them to redo it. */
    data class ReplaceableBaseChanged(val expected: String?, val actual: String?) : RefuseReason()

    companion object {
        internal fun from(ffi: FfiRefuseReason): RefuseReason =
            when (ffi) {
                is FfiRefuseReason.AlreadyExpired -> AlreadyExpired
                is FfiRefuseReason.Tombstoned -> Tombstoned
                is FfiRefuseReason.ReplaceableBaseOnRegularEvent -> ReplaceableBaseOnRegularEvent
                is FfiRefuseReason.ReplaceableBaseChanged ->
                    ReplaceableBaseChanged(ffi.expected, ffi.actual)
            }
    }
}

/** The whole-write terminal. Exactly one of these ends every receipt stream,
 * so a stream can never end in silence and you can always tell a finished
 * write from a dropped subscription. */
sealed class WriteOutcome {
    /** The destination set is CLOSED and every relay in it is terminal. What
     * happened at each is the per-relay facts; this says only that no more
     * are coming. */
    object Settled : WriteOutcome()

    /** Routing finished -- knowledge is exhausted -- and named zero relays.
     * Terminal: there is nowhere to publish. Distinct from a route still
     * resolving, which parks forever. */
    object NoDestination : WriteOutcome()

    data class NotSent(val reason: NotSentReason) : WriteOutcome()

    /** The store answered the acceptance instruction with a semantic no. The
     * write is in custody as a permanently-failed entry: one row, payload
     * intact, readable and removable through [NMPEngine.publishQueue]. */
    data class Refused(val reason: RefuseReason) : WriteOutcome()

    companion object {
        internal fun from(ffi: FfiWriteOutcome): WriteOutcome =
            when (ffi) {
                is FfiWriteOutcome.Settled -> Settled
                is FfiWriteOutcome.NoDestination -> NoDestination
                is FfiWriteOutcome.NotSent -> NotSent(NotSentReason.from(ffi.reason))
                is FfiWriteOutcome.Refused -> Refused(RefuseReason.from(ffi.reason))
            }
    }
}

/** One fact about a write, delivered on its receipt stream.
 *
 * Acceptance is deliberately ABSENT: `publish` returning a receipt IS
 * acceptance, so you never ask the stream whether your write was taken.
 * Settlement is INSPECTED, never AWAITED -- a locally accepted write is
 * already visible through your own live query, reporting cache and zero
 * relays. Never block a UI on this. */
sealed class WriteFact {
    data class Signing(val state: SigningState) : WriteFact()

    data class Relay(val relay: String, val state: RelayState) : WriteFact()

    /** The relays this write is INTENDED for, and whether resolution can
     * still change its mind. [complete] flips on settled RESOLUTION, never on
     * delivery, so `complete == true` with nothing published yet is
     * "sending 0 of n". This is the settlement denominator.
     *
     * `complete == false` with an empty set is a write still learning where
     * it goes; it parks indefinitely and NOTHING expires it. `complete ==
     * true` with an empty set is [WriteOutcome.NoDestination].
     *
     * [awaitingAuthorRoutes] is WHY resolution is still open, as 64-char hex
     * public keys rather than as a sentence: every author whose routes this
     * write is still waiting on, in sorted key order. A later positive route
     * fact for any one of them is the only thing that can move the picture,
     * so the set is both the reason to show and the list of repairs.
     * Non-empty implies `complete == false`; a settled resolution names
     * nobody. The converse does NOT hold: an open picture naming nobody is a
     * write whose routing has not run at all because it is not signed yet,
     * and [Signing] is the fact that says what it IS held on. Never a
     * rendered message -- a park you can only print is a park you cannot act
     * on. */
    data class Destinations(
        val relays: List<String>,
        val complete: Boolean,
        val awaitingAuthorRoutes: List<String>,
    ) : WriteFact()

    data class Outcome(val outcome: WriteOutcome) : WriteFact()

    companion object {
        fun from(ffi: FfiWriteFact): WriteFact =
            when (ffi) {
                is FfiWriteFact.Signing -> Signing(SigningState.from(ffi.state))
                is FfiWriteFact.Relay -> Relay(ffi.relay, RelayState.from(ffi.state))
                is FfiWriteFact.Destinations ->
                    Destinations(ffi.relays, ffi.complete, ffi.awaitingAuthorRoutes)
                is FfiWriteFact.Outcome -> Outcome(WriteOutcome.from(ffi.outcome))
            }
    }
}

/** One write in your publish queue, as you read it back.
 *
 * Enumerating the queue answers "what have I got outstanding, and what went
 * wrong with it" without having held a receipt stream open since acceptance.
 * It is INSPECTION: nothing here blocks. */
data class PublishQueueEntry(
    val receiptId: ULong,
    /** The frozen event id (64-char hex) -- the write's identity from
     * acceptance onward, unchanged by signing. */
    val eventId: String,
    /** The identity frozen at acceptance (64-char hex). Never re-resolved. */
    val pubkey: String,
    val acceptedAt: ULong,
    val signing: SigningState,
    val relays: List<String>,
    val routeComplete: Boolean,
    val relayStates: List<Pair<String, RelayState>>,
    /** `null` while the write is still in progress. */
    val outcome: WriteOutcome?,
    /** LATCHED. Set the first time local persistence refused a durable fact
     * for this write, and never cleared by a later success -- an operator must
     * not lose the only signal that the disk is failing because a relay acked
     * afterwards. */
    val persistenceFault: String?,
) {
    /** Whether this write will produce another fact. */
    val isTerminal: Boolean get() = outcome != null

    companion object {
        internal fun from(ffi: FfiPublishQueueEntry): PublishQueueEntry =
            PublishQueueEntry(
                receiptId = ffi.receiptId,
                eventId = ffi.eventId,
                pubkey = ffi.pubkey,
                acceptedAt = ffi.acceptedAt,
                signing = SigningState.from(ffi.signing),
                relays = ffi.relays,
                routeComplete = ffi.routeComplete,
                relayStates = ffi.relayStates.map { it.relay to RelayState.from(it.state) },
                outcome = ffi.outcome?.let { WriteOutcome.from(it) },
                persistenceFault = ffi.persistenceFault,
            )
    }
}
