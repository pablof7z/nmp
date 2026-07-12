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
 * side of each case).
 *
 * NOTE: there is deliberately no `InvalidSignedEvent` case anymore -- a
 * `WriteIntent.Signed` event that fails `nostr::Event::verify` is no longer
 * rejected synchronously here (#52 Unit B: the guarantee moved to
 * `nmp-engine`'s acceptance boundary so it holds for every entry point, not
 * only this one). It surfaces on the `publish` `Flow<WriteStatus>` instead,
 * as `WriteStatus.Failed`, the first and only status delivered.
 * Receipt-correlation exhaustion is synchronous because no truthful
 * `Receipt` or status flow can be created without an identity.
 *
 * Also no `SignerHasNoPublicKey` case: `addAccount` goes through
 * `nmp::Engine::add_account`, whose built-in `LocalKeySigner` path always
 * reports a public key -- there is no reachable "signer has no public key"
 * state through this entry point, so an impossible error case is not kept
 * on the public surface just in case. */
sealed class NMPError(message: String) : Exception(message) {
    data class NonIndexableFilterTag(val got: String) :
        NMPError("not indexable as a filter key: $got")
    data class InvalidPublicKey(val got: String) : NMPError("invalid public key: $got")
    data class InvalidEventId(val got: String) : NMPError("invalid event id: $got")
    data class InvalidRelayUrl(val got: String) : NMPError("invalid relay url: $got")
    data class InvalidTag(val got: List<String>) : NMPError("invalid tag: $got")
    object InvalidSecretKey : NMPError("invalid secret key")
    object ReceiptCorrelationIdExhausted :
        NMPError("receipt correlation id namespace exhausted")
    data class StoreOpenFailed(val reason: String) : NMPError("store open failed: $reason")
    data class InvalidSignature(val got: String) : NMPError("invalid signature: $got")
    object EngineClosed : NMPError("engine already shut down")
    /** `decodeNostrEntity`'s input was not valid bech32, had an
     * unrecognized HRP prefix, or had a malformed inner TLV payload (#116). */
    data class InvalidNostrEntity(val reason: String) : NMPError("invalid nostr entity: $reason")
    /** `decodeNostrEntity`'s input decoded to `nsec`/`ncryptsec` -- refused
     * rather than decoded (#116). */
    object NostrEntitySecretKeyRejected :
        NMPError("refusing to decode a secret-key entity")

    /** An `NMPDemand` declared `NMPSourceAuthority.AuthorOutboxes` over a
     * selection whose `authors` field is unbound (#107). */
    object AuthorOutboxesRequiresBoundAuthors :
        NMPError("SourceAuthority.AuthorOutboxes requires a selection whose authors field is bound")

    /** An `NMPDemand` declared `NMPSourceAuthority.Pinned` with an empty
     * relay set (#107 Contract: "the pinned relay set must be nonempty"). */
    object EmptyPinnedRelaySet :
        NMPError("SourceAuthority.Pinned requires a nonempty relay set")

    companion object {
        fun from(ffi: FfiException): NMPError =
            when (ffi) {
                is FfiException.NonIndexableFilterTag -> NonIndexableFilterTag(ffi.got)
                is FfiException.InvalidPublicKey -> InvalidPublicKey(ffi.got)
                is FfiException.InvalidEventId -> InvalidEventId(ffi.got)
                is FfiException.InvalidRelayUrl -> InvalidRelayUrl(ffi.got)
                is FfiException.InvalidTag -> InvalidTag(ffi.got)
                is FfiException.InvalidSecretKey -> InvalidSecretKey
                is FfiException.ReceiptCorrelationIdExhausted -> ReceiptCorrelationIdExhausted
                is FfiException.StoreOpenFailed -> StoreOpenFailed(ffi.reason)
                is FfiException.InvalidSignature -> InvalidSignature(ffi.got)
                is FfiException.EngineClosed -> EngineClosed
                is FfiException.InvalidNostrEntity -> InvalidNostrEntity(ffi.reason)
                is FfiException.NostrEntitySecretKeyRejected -> NostrEntitySecretKeyRejected
                is FfiException.AuthorOutboxesRequiresBoundAuthors -> AuthorOutboxesRequiresBoundAuthors
                is FfiException.EmptyPinnedRelaySet -> EmptyPinnedRelaySet
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
