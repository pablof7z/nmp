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
 * on its own dispatcher. That is what makes a signer that takes ten seconds
 * because a person is looking at a confirmation screen an ordinary case
 * rather than a stalled engine.
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

    /** The requests as a cold [Flow], ending when the mailbox does. */
    fun requests(): Flow<NMPSignatureRequest> =
        flow {
            while (true) {
                emit(next() ?: break)
            }
        }

    /**
     * Stop accepting requests and end a parked [next].
     *
     * This does NOT remove the registration: writes for this key then park on
     * an unavailable signer, exactly as they do before any signer attaches.
     * [NMPEngine.removeSigner] removes it.
     */
    fun close() = ffi.close()
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
