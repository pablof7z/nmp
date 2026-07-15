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
 */
sealed class NMPError(message: String) : Exception(message) {
    data class NonIndexableFilterTag(val got: String) :
        NMPError("not indexable as a filter key: $got")
    data class InvalidPublicKey(val got: String) : NMPError("invalid public key: $got")
    data class InvalidEventId(val got: String) : NMPError("invalid event id: $got")
    data class InvalidRelayUrl(val got: String) : NMPError("invalid relay url: $got")
    data class InvalidTag(val got: List<String>) : NMPError("invalid tag: $got")
    object InvalidSecretKey : NMPError("invalid secret key")
    data class InvalidSigner(val reason: String) : NMPError("invalid signer: $reason")
    object NoActiveSigner : NMPError("the active account has no registered signer")
    data class InvalidSignRequest(val reason: String) : NMPError("invalid sign request: $reason")
    data class SignerUnavailable(val reason: String) : NMPError("signer unavailable: $reason")
    data class SignerRejected(val reason: String) : NMPError("signer rejected request: $reason")
    data class InvalidSignerOutput(val reason: String) :
        NMPError("signer returned invalid output: $reason")
    object ReceiptCorrelationIdExhausted :
        NMPError("receipt correlation id namespace exhausted")
    data class StoreOpenFailed(val reason: String) : NMPError("store open failed: $reason")
    data class StoreResetFailed(val reason: String) : NMPError("store reset failed: $reason")
    data class StoreStillOpen(val path: String) : NMPError("persistent store is still open: $path")
    data class ThreadUnavailable(val component: String, val reason: String) :
        NMPError("$component thread unavailable: $reason")
    data class ExecutorSaturated(val component: String, val capacity: ULong) :
        NMPError("$component refused: native task executor is at capacity $capacity")
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

    /** #156: `groupMessageIntent` has no active account from which NMP can
     * derive the unsigned event author. */
    object NoActiveAccount : NMPError("group messages require an active account")

    /** #115: `publishComposed` was called a second time on the same
     * `GroupSendIntent` -- it is take-once by design (call
     * `groupMessageIntent` again for a retry). */
    object IntentAlreadyConsumed :
        NMPError("this composed write intent was already published once")

    data class RelayInformationUnavailable(val kind: RelayInformationErrorKind) :
        NMPError("relay information unavailable: ${kind.describe()}")

    data class RelayInformationWaitersSaturated(val capacity: ULong) :
        NMPError("relay information refused: per-relay waiter capacity $capacity is full")

    companion object {
        fun from(ffi: FfiException): NMPError =
            when (ffi) {
                is FfiException.NonIndexableFilterTag -> NonIndexableFilterTag(ffi.got)
                is FfiException.InvalidPublicKey -> InvalidPublicKey(ffi.got)
                is FfiException.InvalidEventId -> InvalidEventId(ffi.got)
                is FfiException.InvalidRelayUrl -> InvalidRelayUrl(ffi.got)
                is FfiException.InvalidTag -> InvalidTag(ffi.got)
                is FfiException.InvalidSecretKey -> InvalidSecretKey
                is FfiException.InvalidSigner -> InvalidSigner(ffi.reason)
                is FfiException.NoActiveSigner -> NoActiveSigner
                is FfiException.InvalidSignRequest -> InvalidSignRequest(ffi.reason)
                is FfiException.ReceiptCorrelationIdExhausted -> ReceiptCorrelationIdExhausted
                is FfiException.StoreOpenFailed -> StoreOpenFailed(ffi.reason)
                is FfiException.StoreResetFailed -> StoreResetFailed(ffi.reason)
                is FfiException.StoreStillOpen -> StoreStillOpen(ffi.path)
                is FfiException.ThreadUnavailable -> ThreadUnavailable(ffi.component, ffi.reason)
                is FfiException.ExecutorSaturated -> ExecutorSaturated(ffi.component, ffi.capacity)
                is FfiException.InvalidSignature -> InvalidSignature(ffi.got)
                is FfiException.EngineClosed -> EngineClosed
                is FfiException.InvalidNostrEntity -> InvalidNostrEntity(ffi.reason)
                is FfiException.NostrEntitySecretKeyRejected -> NostrEntitySecretKeyRejected
                is FfiException.AuthorOutboxesRequiresBoundAuthors -> AuthorOutboxesRequiresBoundAuthors
                is FfiException.EmptyPinnedRelaySet -> EmptyPinnedRelaySet
                is FfiException.NoActiveAccount -> NoActiveAccount
                is FfiException.IntentAlreadyConsumed -> IntentAlreadyConsumed
                is FfiException.RelayInformationUnavailable ->
                    RelayInformationUnavailable(RelayInformationErrorKind.from(ffi.kind))
                is FfiException.RelayInformationWaitersSaturated ->
                    RelayInformationWaitersSaturated(ffi.capacity)
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

/** Async counterpart for generated UniFFI suspend operations. */
internal suspend inline fun <T> nmpRethrowingAsync(
    crossinline body: suspend () -> T,
): T =
    try {
        body()
    } catch (e: FfiException) {
        throw NMPError.from(e)
    }
