package com.nmp.sdk

import java.util.concurrent.atomic.AtomicBoolean
import uniffi.nmp_ffi.FfiAccountRegistration
import uniffi.nmp_ffi.FfiAuthPolicyCallback
import uniffi.nmp_ffi.FfiAuthPolicyCompletion
import uniffi.nmp_ffi.FfiAuthPolicyCompletionException
import uniffi.nmp_ffi.FfiAuthPolicyOutcome
import uniffi.nmp_ffi.FfiAuthPolicyRegistration
import uniffi.nmp_ffi.FfiAuthPolicyRequest

/** Opaque proof for one exact local-account installation. */
class NMPAccountRegistration internal constructor(
    internal val ffi: FfiAccountRegistration,
) {
    /** The installed account's hex public key. The capability instance stays opaque. */
    val publicKey: String = ffi.publicKey()
}

/** Opaque proof for one exact AUTH-policy installation. */
class NMPAuthPolicyRegistration internal constructor(
    internal val ffi: FfiAuthPolicyRegistration,
) {
    /** The account identity this exact policy installation is bound to. */
    val publicKey: String = ffi.expectedPublicKey()
}

/** Immutable facts for one exact relay AUTH challenge. */
data class NMPAuthPolicyRequest(
    val publicKey: String,
    val relay: String,
    val challenge: String,
    val transportGeneration: ULong,
    val epochSequence: ULong,
)

/** The policy decision delivered through [NMPAuthPolicyCompletion]. */
sealed interface NMPAuthPolicyOutcome {
    data object Allow : NMPAuthPolicyOutcome

    data class Deny(val reason: String) : NMPAuthPolicyOutcome

    data object Unavailable : NMPAuthPolicyOutcome

    data class Technical(val reason: String) : NMPAuthPolicyOutcome
}

/** The only terminal errors an AUTH completion can report. */
sealed class NMPAuthPolicyCompletionError(message: String) : Exception(message) {
    class AlreadyCompleted : NMPAuthPolicyCompletionError("AUTH policy request already completed")

    class Cancelled : NMPAuthPolicyCompletionError("AUTH policy request was cancelled")

    class ReceiverGone : NMPAuthPolicyCompletionError("AUTH policy receiver is gone")
}

internal interface AuthPolicyCompletionPeer {
    fun resolve(outcome: FfiAuthPolicyOutcome)

    fun close()
}

private class FfiAuthPolicyCompletionPeer(
    private val ffi: FfiAuthPolicyCompletion,
) : AuthPolicyCompletionPeer {
    override fun resolve(outcome: FfiAuthPolicyOutcome) = ffi.resolve(outcome)

    override fun close() = ffi.close()
}

/**
 * Completion-only control for one AUTH request.
 *
 * Resolve synchronously for an inline decision. To decide asynchronously, call [retain] during
 * [NMPAuthPolicy.evaluate] and keep this object alive until resolving it. If evaluation returns
 * without either resolving or retaining the completion, NMP deterministically treats the policy as
 * unavailable; this explicit handshake avoids making correctness depend on JVM garbage collection.
 */
class NMPAuthPolicyCompletion internal constructor(
    private val peer: AuthPolicyCompletionPeer,
) {
    private val retained = AtomicBoolean(false)

    /** Mark this exact completion as pending beyond the evaluation callback. */
    fun retain(): NMPAuthPolicyCompletion {
        retained.set(true)
        return this
    }

    /** Resolve this exact request once. */
    @Throws(NMPAuthPolicyCompletionError::class)
    fun resolve(outcome: NMPAuthPolicyOutcome) {
        try {
            peer.resolve(outcome.toFfi())
        } catch (_: FfiAuthPolicyCompletionException.AlreadyCompleted) {
            throw NMPAuthPolicyCompletionError.AlreadyCompleted()
        } catch (_: FfiAuthPolicyCompletionException.Cancelled) {
            throw NMPAuthPolicyCompletionError.Cancelled()
        } catch (_: FfiAuthPolicyCompletionException.ReceiverGone) {
            throw NMPAuthPolicyCompletionError.ReceiverGone()
        } catch (_: IllegalStateException) {
            // The callback scope released a completion that was neither resolved nor retained.
            throw NMPAuthPolicyCompletionError.ReceiverGone()
        }
    }

    internal fun releaseCallbackScope() {
        if (!retained.get()) peer.close()
    }
}

/** Completion-only AUTH policy. No synchronous decision is returned from [evaluate]. */
interface NMPAuthPolicy {
    fun evaluate(
        request: NMPAuthPolicyRequest,
        completion: NMPAuthPolicyCompletion,
    )

    /** Called exactly once after cancellation wins. It is safe to reenter [NMPEngine]. */
    fun onCancelled(request: NMPAuthPolicyRequest) {}
}

internal class NMPAuthPolicyBridge(
    private val policy: NMPAuthPolicy,
) : FfiAuthPolicyCallback {
    override fun evaluate(
        request: FfiAuthPolicyRequest,
        completion: FfiAuthPolicyCompletion,
    ) {
        val nativeCompletion =
            NMPAuthPolicyCompletion(FfiAuthPolicyCompletionPeer(completion))
        try {
            policy.evaluate(request.toNative(), nativeCompletion)
        } finally {
            nativeCompletion.releaseCallbackScope()
        }
    }

    override fun onCancelled(request: FfiAuthPolicyRequest) {
        policy.onCancelled(request.toNative())
    }
}

private fun FfiAuthPolicyRequest.toNative() =
    NMPAuthPolicyRequest(
        publicKey = expectedPublicKey,
        relay = relay,
        challenge = challenge,
        transportGeneration = transportGeneration,
        epochSequence = epochSequence,
    )

private fun NMPAuthPolicyOutcome.toFfi(): FfiAuthPolicyOutcome =
    when (this) {
        NMPAuthPolicyOutcome.Allow -> FfiAuthPolicyOutcome.Allow
        is NMPAuthPolicyOutcome.Deny -> FfiAuthPolicyOutcome.Deny(reason)
        NMPAuthPolicyOutcome.Unavailable -> FfiAuthPolicyOutcome.Unavailable
        is NMPAuthPolicyOutcome.Technical -> FfiAuthPolicyOutcome.Technical(reason)
    }
