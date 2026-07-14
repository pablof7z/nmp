package com.nmp.sdk

import java.util.concurrent.atomic.AtomicReference
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.suspendCancellableCoroutine
import uniffi.nmp_ffi.FfiSignEventFailure
import uniffi.nmp_ffi.FfiSignedEvent
import uniffi.nmp_ffi.FfiSignEventRequest
import uniffi.nmp_ffi.NmpEngineInterface
import uniffi.nmp_ffi.SignEventObserver

private enum class SignEventTerminal {
    OPEN,
    COMPLETED,
    CANCELLED,
}

/** Immutable sign-only event body; NMP freezes its author from active identity. */
data class NMPUnsignedEvent(
    val createdAt: ULong,
    val kind: UShort,
    val tags: List<List<String>>,
    val content: String,
) {
    internal fun toFfi() = FfiSignEventRequest(createdAt, kind, tags, content)
}

/** Verified sign-only result; it carries no storage or publication claim. */
data class NMPSignedEvent(
    val id: String,
    val pubkey: String,
    val createdAt: ULong,
    val kind: UShort,
    val tags: List<List<String>>,
    val content: String,
    val signature: String,
) {
    internal constructor(ffi: FfiSignedEvent) : this(
        ffi.id,
        ffi.pubkey,
        ffi.createdAt,
        ffi.kind,
        ffi.tags,
        ffi.content,
        ffi.sig,
    )
}

internal suspend fun signEvent(
    engine: NmpEngineInterface,
    event: NMPUnsignedEvent,
): NMPSignedEvent = signEvent(event) { request, observer ->
    val handle = nmpRethrowing { engine.signEvent(request, observer) }
    val cancel: () -> Unit = { handle.cancel() }
    cancel
}

internal suspend fun signEvent(
    event: NMPUnsignedEvent,
    start: (FfiSignEventRequest, SignEventObserver) -> (() -> Unit),
): NMPSignedEvent =
    suspendCancellableCoroutine { continuation ->
        val cancelOperation = AtomicReference<(() -> Unit)?>(null)
        val terminal = AtomicReference(SignEventTerminal.OPEN)

        fun complete(result: Result<NMPSignedEvent>) {
            if (terminal.compareAndSet(SignEventTerminal.OPEN, SignEventTerminal.COMPLETED)) {
                cancelOperation.set(null)
                continuation.resumeWith(result)
            }
        }

        val observer =
            object : SignEventObserver {
                override fun onSigned(event: FfiSignedEvent) {
                    complete(Result.success(NMPSignedEvent(event)))
                }

                override fun onFailed(failure: FfiSignEventFailure) {
                    val error =
                        when (failure) {
                            is FfiSignEventFailure.SignerUnavailable ->
                                NMPError.SignerUnavailable(failure.reason)
                            is FfiSignEventFailure.SignerRejected ->
                                NMPError.SignerRejected(failure.reason)
                            is FfiSignEventFailure.InvalidSignerOutput ->
                                NMPError.InvalidSignerOutput(failure.reason)
                            is FfiSignEventFailure.Cancelled ->
                                CancellationException("sign operation cancelled")
                        }
                    complete(Result.failure(error))
                }
            }

        continuation.invokeOnCancellation {
            if (terminal.compareAndSet(SignEventTerminal.OPEN, SignEventTerminal.CANCELLED)) {
                cancelOperation.getAndSet(null)?.invoke()
            }
        }

        try {
            val cancel = start(event.toFfi(), observer)
            cancelOperation.set(cancel)
            when (terminal.get()) {
                SignEventTerminal.OPEN -> Unit
                SignEventTerminal.COMPLETED -> cancelOperation.set(null)
                SignEventTerminal.CANCELLED -> cancelOperation.getAndSet(null)?.invoke()
            }
        } catch (error: Throwable) {
            complete(Result.failure(error))
        }
    }
