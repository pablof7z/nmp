package com.nmp.sdk

import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.flow
import uniffi.nmp_ffi.FfiSignatureSettleException
import uniffi.nmp_ffi.FfiSignerRejection
import uniffi.nmp_ffi.FfiUnsignedEvent
import uniffi.nmp_ffi.NmpSignatureRequest
import uniffi.nmp_ffi.NmpSignerMailbox

/**
 * The app-supplied signer door (#1238).
 *
 * Before this file existed, a Kotlin or Swift app could register no signer at
 * all: the only identity it could give NMP was a raw secret key handed to
 * `addAccount`, because `Engine::add_signer` is generic over a Rust trait and
 * cannot cross UniFFI. Anything else an app might sign with -- a Keystore key
 * it will not surrender, a NIP-55 external signer app, a remote bunker, a
 * hardware device -- had nowhere to attach.
 *
 * The door is a stream, not a callback. NMP does not call into the app
 * (#783): it enqueues immutable requests and returns, and the app drains them
 * on its own dispatcher. The reason is not that a person is slow -- NMP's
 * ready-or-pending capability shape already absorbs human time without
 * holding anything, which is what the AUTH policy does today. It is that #783
 * requires it, that the one remaining callback is invoked with the capability
 * mutex held on a shared-runtime task, and that making "NMP calls you" safe
 * across UniFFI cost a 752-line five-state Condvar linearization this door
 * does not need.
 *
 * Cancellation is deliberately NOT bridged the way the row flow bridges it.
 * A query handle's `cancel` ends one stream; this mailbox IS the app's
 * signer, so a `finally { cancel() }` would silently park every later write
 * for that key the first time a collecting scope died. Kotlin needs no bridge
 * at all -- UniFFI's generated `suspendCancellableCoroutine` frees the parked
 * Rust future -- while Swift needs [NMPSignerMailbox.unpark], the
 * non-destructive wake this file also exposes.
 */

/**
 * One unsigned event NMP needs a signature for.
 *
 * [pubkey] is frozen by the write that asked for it: sign as that key or
 * refuse. A signature by any other key fails the write at NMP's promotion
 * boundary rather than publishing something unintended.
 */
data class NMPSignatureRequestBody(
    val pubkey: String,
    val createdAt: ULong,
    val kind: UShort,
    val tags: List<List<String>>,
    val content: String,
) {
    internal constructor(ffi: FfiUnsignedEvent) : this(
        ffi.pubkey,
        ffi.createdAt,
        ffi.kind,
        ffi.tags,
        ffi.content,
    )
}

/**
 * The closed set of refusals an app can give for one signature request.
 *
 * Deliberately narrower than NMP's own signer failure vocabulary: a timeout
 * or a disconnect is something NMP concludes about a signer, not something a
 * signer says about itself.
 */
sealed class NMPSignerRejection {
    /** The person said no. Terminal -- retrying cannot change a decision. */
    data class Rejected(val reason: String) : NMPSignerRejection()

    /** The signer cannot answer right now. Retryable; the write parks. */
    object Unavailable : NMPSignerRejection()

    internal fun toFfi(): FfiSignerRejection =
        when (this) {
            is Rejected -> FfiSignerRejection.Rejected(reason)
            is Unavailable -> FfiSignerRejection.Unavailable
        }
}

/** Why settling a signature request did not take effect. */
sealed class NMPSignatureSettleError(message: String) : Exception(message) {
    /**
     * The request was cancelled, or the write that asked for it went away.
     * The answer is discarded; the mailbox is unaffected.
     */
    class NoLongerAwaited :
        NMPSignatureSettleError("the engine was no longer awaiting this signature request")

    /** Already settled. Each request carries exactly one answer. */
    class AlreadySettled : NMPSignatureSettleError("this signature request was already settled")

    /**
     * `id`, `pubkey` or `sig` was not the fixed-width hex the protocol
     * defines. The request is NOT spent -- correct the value and settle again.
     */
    class MalformedSignedEvent(reason: String) :
        NMPSignatureSettleError("the signed event could not be parsed: $reason")
}

/**
 * One signature NMP is waiting for.
 *
 * Settles exactly once: a second [resolve] or [reject] throws
 * [NMPSignatureSettleError.AlreadySettled] rather than delivering a second
 * answer. Letting it be collected without settling is a legal answer too --
 * NMP hears the ordinary retryable unavailable and the write parks, which is
 * what an app whose signer went away should say.
 */
class NMPSignatureRequest internal constructor(
    private val ffi: NmpSignatureRequest,
) {
    /** The exact body to sign. */
    val body: NMPSignatureRequestBody = NMPSignatureRequestBody(ffi.unsignedEvent())

    /** Answer with a signature over exactly [body]. */
    @Throws(NMPSignatureSettleError::class)
    fun resolve(signed: NMPSignedEvent) =
        settling { ffi.resolve(signed.toFfi()) }

    /** Answer with a refusal. */
    @Throws(NMPSignatureSettleError::class)
    fun reject(reason: NMPSignerRejection) =
        settling { ffi.reject(reason.toFfi()) }

    private inline fun settling(body: () -> Unit) {
        try {
            body()
        } catch (_: FfiSignatureSettleException.NoLongerAwaited) {
            throw NMPSignatureSettleError.NoLongerAwaited()
        } catch (_: FfiSignatureSettleException.AlreadySettled) {
            throw NMPSignatureSettleError.AlreadySettled()
        } catch (failure: FfiSignatureSettleException.MalformedSignedEvent) {
            throw NMPSignatureSettleError.MalformedSignedEvent(failure.reason)
        }
    }
}

/**
 * The app's end of one registered signer: a stream of signature requests, and
 * the exact-instance proof that removes the registration.
 *
 * Drain it from a long-lived coroutine:
 *
 * ```kotlin
 * val mailbox = engine.addSigner(pubkey)
 * scope.launch {
 *     mailbox.requests().collect { request ->
 *         runCatching { request.resolve(myKeystoreSigner.sign(request.body)) }
 *             .onFailure { request.reject(NMPSignerRejection.Unavailable) }
 *     }
 * }
 * ```
 *
 * Exactly one drainer at a time: two concurrent [next] calls would each
 * believe they held the only copy of a take-once answer, so the second is
 * refused rather than silently losing a request.
 */
class NMPSignerMailbox internal constructor(
    internal val ffi: NmpSignerMailbox,
) {
    /** The key this mailbox signs for. */
    val publicKey: String = ffi.publicKey()

    /**
     * Await the next signature request, or `null` once the mailbox is closed
     * and drained.
     */
    suspend fun next(): NMPSignatureRequest? =
        nmpRethrowing { ffi.next() }?.let(::NMPSignatureRequest)

    /**
     * The requests as a cold [Flow], ending when the mailbox does -- or when
     * the collecting scope is cancelled, which ends only this collection.
     *
     * There is deliberately no `finally { cancel() }` here, unlike the row
     * flow. A query handle's cancel ends one stream; this mailbox IS the
     * app's signer, so tying it to a collecting scope would silently park
     * every later write for this key the first time a screen went away. Nor
     * is a `finally { unpark() }` needed: UniFFI's generated Kotlin parks in
     * `suspendCancellableCoroutine` and frees the Rust future in its own
     * `finally`, so a cancelled collection already releases the single-reader
     * claim and the registration simply stays live. (Swift has no such luck
     * -- its generated parking is `withUnsafeContinuation` -- which is why
     * [unpark] exists at all.)
     *
     * The one visible cost: a request already lifted out of the queue when
     * the collector dies is dropped, which the engine hears as the ordinary
     * retryable unavailable and the write parks for the next drain.
     */
    fun requests(): Flow<NMPSignatureRequest> =
        flow {
            while (true) {
                emit(next() ?: break)
            }
        }

    /**
     * Stop accepting requests and end a parked [next].
     *
     * Destructive, and the only destructive verb here: this app stops being
     * the signer for [publicKey], so writes for that key park on an
     * unavailable signer exactly as they do before any signer attaches. It
     * does NOT remove the registration -- [NMPEngine.removeSigner] does. To
     * end a drain without giving up the signer, cancel the collecting scope
     * (or call [unpark]).
     */
    fun cancel() = ffi.cancel()

    /**
     * End one [next] without closing the mailbox. A parked drain ends now;
     * with none parked the next [next] ends instead. Everything else -- the
     * registration, the queued requests, the ability to sign -- survives.
     */
    fun unpark() = ffi.unpark()
}

internal fun NMPSignedEvent.toFfi() =
    uniffi.nmp_ffi.FfiSignedEvent(id, pubkey, createdAt, kind, tags, content, signature)

/**
 * Register a signing capability this app owns, for exactly [publicKey].
 *
 * `addAccount` is the door for a key NMP holds; it takes the secret. This one
 * takes no secret -- only the public key the app can sign for -- and returns
 * the mailbox of requests to drain. Registering does not make the key active;
 * use `setActiveAccount` for that. Registering the same key again replaces the
 * capability and invalidates the previous mailbox's registration.
 */
fun NMPEngine.addSigner(publicKey: String): NMPSignerMailbox =
    NMPSignerMailbox(nmpRethrowing { ffi.addSignerMailbox(publicKey) })

/**
 * Remove only the signer installation proven by [mailbox]. Repeated or stale
 * removal returns `false` and can never detach a replacement registered for
 * the same key.
 */
fun NMPEngine.removeSigner(mailbox: NMPSignerMailbox): Boolean =
    nmpRethrowing { ffi.removeSignerMailbox(mailbox.ffi) }
