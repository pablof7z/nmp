// The one-shot engine-authorized Blossom upload (#971), mirroring
// BlossomUpload.swift exactly.
//
// `Blossom.kt` next door is the LOW-LEVEL projection: the app builds a
// kind:24242 draft, gets it signed, validates it, and drives `BlossomClient`
// itself. This file is the opposite bargain, and the one an app should
// normally use: state what you are uploading and where, and NMP owns the
// author, the clock, the sha256, the BUD-11 composition, the signature, the
// re-validation and the request.
//
// Nothing here accepts or returns an author pubkey, event kind, tag, unsigned
// event, sign request, signed authorization, caller timestamp, expiration or
// blob hash. The result is the same `BlobDescriptor` the low-level upload
// returns; there is no second verified-asset type.

package com.nmp.sdk

import kotlinx.coroutines.CancellationException
import uniffi.nmp_ffi.FfiBlossomUploadFailure
import uniffi.nmp_ffi.FfiBlossomUploadRequest
import uniffi.nmp_ffi.NmpEngineInterface

/** The engine-authorized upload's exhaustive failure taxonomy
 * (`FfiBlossomUploadFailure` mirror).
 *
 * Deliberately DISTINCT from [BlossomUploadError], which belongs to the
 * low-level client: that operation cannot fail for a signer, clock or
 * active-account reason, and this one cannot fail for an authorization the
 * caller supplied. Nothing is flattened into a message string.
 *
 * There is no `Cancelled` case: a withdrawn upload surfaces as a coroutine
 * [CancellationException], the same way the sign-only door reports it. */
sealed class BlossomUploadFailure(message: String) : Exception(message) {
    data class InvalidServerUrl(val error: BlossomServerUrlError) :
        BlossomUploadFailure("invalid Blossom server URL: $error")

    class EmptyContentType : BlossomUploadFailure("Blossom upload content type is empty")

    /** NMP could not compose a representable BUD-11 window at this instant. */
    data class AuthorizationWindow(
        val createdAt: ULong,
        val lifetimeSeconds: ULong,
    ) : BlossomUploadFailure(
            "no representable BUD-11 authorization window at $createdAt for a " +
                "${lifetimeSeconds}s lifetime",
        )

    class NoActiveSigner : BlossomUploadFailure("no active signer")

    data class SignerUnavailable(val reason: String) :
        BlossomUploadFailure("signer unavailable: $reason")

    data class SignerRejected(val reason: String) :
        BlossomUploadFailure("signer rejected request: $reason")

    data class InvalidSignerOutput(val reason: String) :
        BlossomUploadFailure("signer returned invalid output: $reason")

    data class AuthorizationExpired(val expiration: ULong, val now: ULong) :
        BlossomUploadFailure("Blossom authorization expired at $expiration; now is $now")

    /** The engine clock moved backwards between composing the authorization
     * and validating it. A clock fact, not a signer fault. */
    data class ClockMovedBackward(val createdAt: ULong, val now: ULong) :
        BlossomUploadFailure("clock moved backward: created_at $createdAt, now $now")

    data class ClientBuild(val reason: String) :
        BlossomUploadFailure("Blossom HTTP client construction failed: $reason")

    data class LocalHostNotAdmitted(val host: String) :
        BlossomUploadFailure("refusing Blossom upload: host $host is local and not opted-in")

    data class Network(val detail: String) :
        BlossomUploadFailure("Blossom upload transport failed: $detail")

    data class RedirectRefused(val status: UShort) :
        BlossomUploadFailure("Blossom upload redirects are not followed (HTTP $status)")

    data class AuthRejected(val status: UShort, val reason: String?) :
        BlossomUploadFailure("Blossom server rejected the authorization (HTTP $status: $reason)")

    data class ServerRejected(val status: UShort, val reason: String?) :
        BlossomUploadFailure("Blossom server rejected the upload (HTTP $status: $reason)")

    data class ServerError(val status: UShort, val reason: String?) :
        BlossomUploadFailure("Blossom server failed (HTTP $status: $reason)")

    data class ResponseTooLarge(val limitBytes: ULong) :
        BlossomUploadFailure("Blossom descriptor response exceeds $limitBytes bytes")

    data class DescriptorInvalid(val error: BlossomDescriptorError) :
        BlossomUploadFailure("Blossom descriptor invalid: $error")

    data class Sha256Mismatch(
        val expectedSha256Hex: String,
        val returnedSha256Hex: String,
    ) : BlossomUploadFailure(
            "Blossom server returned sha256 $returnedSha256Hex for a blob hashing to " +
                "$expectedSha256Hex -- refusing the descriptor",
        )

    class EngineClosed : BlossomUploadFailure("engine is closed")

    /** The one-shot result was already delivered to a prior await. */
    class AlreadyConsumed :
        BlossomUploadFailure("Blossom upload result was already delivered to a prior await")

    companion object {
        /** `null` for `Cancelled`, which is a coroutine cancellation rather
         * than a Blossom fault. */
        internal fun from(ffi: FfiBlossomUploadFailure): BlossomUploadFailure? =
            when (ffi) {
                is FfiBlossomUploadFailure.InvalidServerUrl ->
                    InvalidServerUrl(BlossomServerUrlError.from(ffi.error))
                is FfiBlossomUploadFailure.EmptyContentType -> EmptyContentType()
                is FfiBlossomUploadFailure.AuthorizationWindow ->
                    AuthorizationWindow(ffi.createdAtSecs, ffi.lifetimeSecs)
                is FfiBlossomUploadFailure.NoActiveSigner -> NoActiveSigner()
                is FfiBlossomUploadFailure.SignerUnavailable -> SignerUnavailable(ffi.reason)
                is FfiBlossomUploadFailure.SignerRejected -> SignerRejected(ffi.reason)
                is FfiBlossomUploadFailure.InvalidSignerOutput -> InvalidSignerOutput(ffi.reason)
                is FfiBlossomUploadFailure.AuthorizationExpired ->
                    AuthorizationExpired(ffi.expirationSecs, ffi.nowSecs)
                is FfiBlossomUploadFailure.ClockMovedBackward ->
                    ClockMovedBackward(ffi.createdAtSecs, ffi.nowSecs)
                is FfiBlossomUploadFailure.ClientBuild -> ClientBuild(ffi.reason)
                is FfiBlossomUploadFailure.LocalHostNotAdmitted -> LocalHostNotAdmitted(ffi.host)
                is FfiBlossomUploadFailure.Network -> Network(ffi.detail)
                is FfiBlossomUploadFailure.RedirectRefused -> RedirectRefused(ffi.status)
                is FfiBlossomUploadFailure.AuthRejected -> AuthRejected(ffi.status, ffi.reason)
                is FfiBlossomUploadFailure.ServerRejected -> ServerRejected(ffi.status, ffi.reason)
                // UniFFI's Kotlin backend rewrites a trailing `Error` to
                // `Exception`, so the Rust `ServerError` variant is generated
                // as `ServerException` on this side only.
                is FfiBlossomUploadFailure.ServerException -> ServerError(ffi.status, ffi.reason)
                is FfiBlossomUploadFailure.ResponseTooLarge -> ResponseTooLarge(ffi.limitBytes)
                is FfiBlossomUploadFailure.DescriptorInvalid ->
                    DescriptorInvalid(BlossomDescriptorError.from(ffi.error))
                is FfiBlossomUploadFailure.Sha256Mismatch ->
                    Sha256Mismatch(ffi.expectedSha256Hex, ffi.returnedSha256Hex)
                is FfiBlossomUploadFailure.EngineClosed -> EngineClosed()
                is FfiBlossomUploadFailure.AlreadyConsumed -> AlreadyConsumed()
                is FfiBlossomUploadFailure.Cancelled -> null
            }
    }
}

/** Translate the engine-authorized upload's typed failure. `Cancelled`
 * becomes a coroutine [CancellationException]: the operation was withdrawn,
 * not a Blossom fault. */
internal fun mapBlossomUploadFailure(failure: FfiBlossomUploadFailure): Throwable =
    BlossomUploadFailure.from(failure)
        ?: CancellationException("Blossom upload cancelled")

/** Upload one blob to a Blossom server, authorized by the active signer.
 *
 * NMP owns the entire transaction inside this one call: it freezes the author
 * from the active account, reads its own clock, hashes these exact bytes once,
 * composes and signs the BUD-11 kind:24242 authorization, re-validates the
 * signature against that exact hash before any HTTP, and performs the hardened
 * `PUT /upload`. The returned descriptor's sha256 has been PROVEN equal to the
 * hash of the bytes that were sent.
 *
 * `handle.cancel()` runs in a `finally` so that cancelling the calling
 * coroutine (which drops the in-flight Rust future) also withdraws the Rust
 * operation -- Kotlin coroutine cancellation never reaches Rust on its own.
 * `cancel()` is idempotent and safe after completion. Cancelling before the
 * request is transmitted means no HTTP happened at all; cancelling AFTER it
 * was transmitted is an observation gap -- the local operation stopped, and
 * NMP does not claim whether the server stored the bytes.
 *
 * Nothing about this is durable: no receipt, no retry owner, no stored row. */
internal suspend fun uploadBlossom(
    engine: NmpEngineInterface,
    serverUrl: String,
    blob: ByteArray,
    contentType: String,
    description: String,
): BlobDescriptor {
    val handle =
        try {
            engine.uploadBlossom(
                FfiBlossomUploadRequest(serverUrl, blob, contentType, description),
            )
        } catch (failure: FfiBlossomUploadFailure) {
            throw mapBlossomUploadFailure(failure)
        }
    try {
        return BlobDescriptor.from(handle.uploaded())
    } catch (failure: FfiBlossomUploadFailure) {
        throw mapBlossomUploadFailure(failure)
    } finally {
        handle.cancel()
    }
}
