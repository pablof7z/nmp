// The ergonomic error surface: `com.nmp.sdk`'s public API never leaks the
// `uniffi.nmp_ffi`-generated `FfiException` type past this file (mirrors
// NMPError.swift's "hide the Ffi* types behind an ergonomic wrapper" rule
// exactly, even though UniFFI's Kotlin codegen already gives `FfiException`
// reasonably ergonomic named subclasses -- keeping one seam, in one file,
// matches every other platform SDK in this repo rather than special-casing
// Kotlin because its generated shape happens to need less help).

package com.nmp.sdk

import uniffi.nmp_ffi.FfiException

/** Every way a call into the engine can fail -- typed states, never a crash
 * (mirrors `nmp-ffi`'s own `FfiError`; see that type's doc for the Rust
 * side of each case). */
sealed class NMPError(message: String) : Exception(message) {
    data class NonIndexableFilterTag(val got: String) :
        NMPError("not indexable as a filter key: $got")
    data class InvalidPublicKey(val got: String) : NMPError("invalid public key: $got")
    data class InvalidEventId(val got: String) : NMPError("invalid event id: $got")
    data class InvalidRelayUrl(val got: String) : NMPError("invalid relay url: $got")
    data class InvalidTag(val got: List<String>) : NMPError("invalid tag: $got")
    object InvalidSecretKey : NMPError("invalid secret key")
    object SignerHasNoPublicKey : NMPError("signer has no public key")
    data class StoreOpenFailed(val reason: String) : NMPError("store open failed: $reason")
    data class InvalidSignature(val got: String) : NMPError("invalid signature: $got")
    data class InvalidSignedEvent(val reason: String) : NMPError("invalid signed event: $reason")

    companion object {
        fun from(ffi: FfiException): NMPError =
            when (ffi) {
                is FfiException.NonIndexableFilterTag -> NonIndexableFilterTag(ffi.got)
                is FfiException.InvalidPublicKey -> InvalidPublicKey(ffi.got)
                is FfiException.InvalidEventId -> InvalidEventId(ffi.got)
                is FfiException.InvalidRelayUrl -> InvalidRelayUrl(ffi.got)
                is FfiException.InvalidTag -> InvalidTag(ffi.got)
                is FfiException.InvalidSecretKey -> InvalidSecretKey
                is FfiException.SignerHasNoPublicKey -> SignerHasNoPublicKey
                is FfiException.StoreOpenFailed -> StoreOpenFailed(ffi.reason)
                is FfiException.InvalidSignature -> InvalidSignature(ffi.got)
                is FfiException.InvalidSignedEvent -> InvalidSignedEvent(ffi.reason)
            }
    }
}

/** Runs `body`, translating any thrown `FfiException` into the ergonomic
 * `NMPError` -- the one seam every call into `uniffi.nmp_ffi` passes
 * through. */
internal inline fun <T> nmpRethrowing(body: () -> T): T =
    try {
        body()
    } catch (e: FfiException) {
        throw NMPError.from(e)
    }
