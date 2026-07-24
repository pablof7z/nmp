package com.nmp.sdk

import kotlinx.coroutines.CancellationException
import uniffi.nmp_ffi.FfiSignEventFailure
import uniffi.nmp_ffi.FfiSignedEvent
import uniffi.nmp_ffi.FfiSignEventRequest
import uniffi.nmp_ffi.NmpEngineInterface

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

/** Sign one exact event through the active signer (#680, #727). Starting the
 * operation either returns a one-shot
 * [uniffi.nmp_ffi.NmpSignEventHandle] or throws a synchronous start refusal;
 * after a handle exists, awaiting its `signed()` yields the fully-verified
 * event or only a typed [FfiSignEventFailure]. There is no
 * `suspendCancellableCoroutine` state
 * machine anymore: the pull IS the await. `handle.cancel()` runs in a
 * `finally` so that cancelling the calling coroutine (which drops the
 * in-flight Rust future) also withdraws the Rust operation -- Kotlin
 * coroutine cancellation never reaches Rust on its own. `cancel()` is
 * idempotent and safe after completion, so the unconditional `finally` is
 * correct on the success and failure paths too. */
internal suspend fun signEvent(
    engine: NmpEngineInterface,
    event: NMPUnsignedEvent,
): NMPSignedEvent {
    val handle = nmpRethrowing { engine.signEvent(event.toFfi()) }
    try {
        return NMPSignedEvent(handle.signed())
    } catch (failure: FfiSignEventFailure) {
        throw mapSignEventFailure(failure)
    } finally {
        handle.cancel()
    }
}

/** Translate the generated one-shot sign failure into the ergonomic surface.
 * `Cancelled` becomes a coroutine [CancellationException] (the operation was
 * withdrawn, not a signer fault); `AlreadyConsumed` is caller misuse -- a
 * second await on a one-shot handle -- surfaced as [IllegalStateException]. */
internal fun mapSignEventFailure(failure: FfiSignEventFailure): Throwable =
    when (failure) {
        is FfiSignEventFailure.SignerUnavailable ->
            NMPError.SignerUnavailable(failure.reason)
        is FfiSignEventFailure.SignerRejected ->
            NMPError.SignerRejected(failure.reason)
        is FfiSignEventFailure.InvalidSignerOutput ->
            NMPError.InvalidSignerOutput(failure.reason)
        is FfiSignEventFailure.Cancelled ->
            CancellationException("sign operation cancelled")
        is FfiSignEventFailure.AlreadyConsumed ->
            IllegalStateException("sign event handle already consumed")
    }
