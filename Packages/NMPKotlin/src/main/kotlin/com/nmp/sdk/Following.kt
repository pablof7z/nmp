// NMP's optional NIP-02 following resource, and the typed follow/unfollow
// action's pre-custody refusal (#1640), in ergonomic Kotlin shape. Mirrors
// Following.swift's `NMPEngine.observeFollowing`/`follow`/`unfollow` PUBLIC
// semantics exactly. No contact-list parsing, replacement composition, or
// readiness policy lives on this side of the FFI boundary -- this file only
// mirrors Rust-owned state and drains Rust-owned streams, exactly like
// `nmp-ffi/src/nip02.rs`'s own header comment says of the Rust side.
//
// The typed current-following read stays a projection over the ordinary
// live-query path (`observeFollowing`); the write side returns the ordinary
// [Receipt] directly -- no follow-only action/status stream, registration
// handle, or second cancellation lifecycle exists at this boundary.
//
// SCOPE NOTE: Following.swift also defines `NMPFollowing`, a `@MainActor`
// `ObservableObject` that bundles `canToggle`/`toggle()` local UI-state
// bookkeeping on top of
// the two APIs below. That class is SwiftUI-specific presentation sugar,
// exactly the same shape as `Observable.swift`'s `NMPQuerySnapshot` and
// `NMPDiagnosticsSnapshotObserver` -- and this codebase's established
// precedent (see that file's own header) is to NOT port those `@Observable`
// convenience wrappers to Kotlin: `Query.kt`/`DiagnosticsQuery.kt` stop at
// the `Flow`-returning primary API and leave the ObservableObject sugar
// unbuilt on this platform. This file follows that same precedent and stops
// at the primary API; it does not invent a StateFlow/ViewModel-shaped
// counterpart to `NMPFollowing`. If Android callers need the toggle/retry
// state machine ported too, that is a separate, explicit follow-up -- not
// guessed here.
package com.nmp.sdk

import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.flow
import uniffi.nmp_ffi.FfiFollowActionException
import uniffi.nmp_ffi.FfiFollowAvailability
import uniffi.nmp_ffi.FfiFollowRelationship
import uniffi.nmp_ffi.FfiFollowSnapshot
import uniffi.nmp_ffi.NmpEngineInterface

/** The current account's relationship to a `target` pubkey, as NMP's own
 * kind:3 projection sees it right now (`FfiFollowRelationship` mirror). */
enum class FollowRelationship {
    Unknown,
    NotFollowing,
    Following,
    ;

    companion object {
        fun from(ffi: FfiFollowRelationship): FollowRelationship =
            when (ffi) {
                FfiFollowRelationship.UNKNOWN -> Unknown
                FfiFollowRelationship.NOT_FOLLOWING -> NotFollowing
                FfiFollowRelationship.FOLLOWING -> Following
            }
    }
}

/** Source evidence for the live relationship projection
 * (`FfiFollowAvailability` mirror). It does not gate follow/unfollow: NMP can
 * write cached state or create the first list while relay truth is incomplete.
 * `Ready` is not global Nostr completeness. */
enum class FollowAvailability {
    SignedOut,
    Acquiring,
    Ready,
    NoContactList,
    CachedOnly,
    SourceUnavailable,
    ;

    companion object {
        fun from(ffi: FfiFollowAvailability): FollowAvailability =
            when (ffi) {
                FfiFollowAvailability.SIGNED_OUT -> SignedOut
                FfiFollowAvailability.ACQUIRING -> Acquiring
                FfiFollowAvailability.READY -> Ready
                FfiFollowAvailability.NO_CONTACT_LIST -> NoContactList
                FfiFollowAvailability.CACHED_ONLY -> CachedOnly
                FfiFollowAvailability.SOURCE_UNAVAILABLE -> SourceUnavailable
            }
    }
}

/** One pushed state of the current account's relationship to `target`
 * (`FfiFollowSnapshot` mirror). Delivered by `NMPEngine.observeFollowing`,
 * pushed reactively, never polled. */
data class FollowingSnapshot(
    val currentPubkey: String?,
    val target: String,
    val relationship: FollowRelationship,
    val availability: FollowAvailability,
    val baseEventId: String?,
) {
    companion object {
        fun from(ffi: FfiFollowSnapshot): FollowingSnapshot =
            FollowingSnapshot(
                currentPubkey = ffi.currentPubkey,
                target = ffi.target,
                relationship = FollowRelationship.from(ffi.relationship),
                availability = FollowAvailability.from(ffi.availability),
                baseEventId = ffi.baseEventId,
            )

        /** The pre-acquisition placeholder a caller may render before the
         * first real snapshot arrives (mirrors
         * `NMPFollowingSnapshot.initial(target:)`). Kotlin's cold `Flow`
         * -- unlike Swift's synchronously-constructed `struct` -- has no
         * value at all until the flow is collected and the engine has
         * delivered one, so a caller that wants an immediate placeholder value
         * (e.g. to seed a `MutableStateFlow` before `collect` starts) uses
         * this explicitly; `observeFollowing` itself only ever emits real,
         * engine-sourced snapshots. */
        fun initial(target: String): FollowingSnapshot =
            FollowingSnapshot(
                currentPubkey = null,
                target = target,
                relationship = FollowRelationship.Unknown,
                availability = FollowAvailability.Acquiring,
                baseEventId = null,
            )
    }
}

/** A typed follow/unfollow action was refused before ordinary receipt
 * custody. [InvalidTarget] is the one refusal this boundary adds: `target`
 * crosses FFI as a caller-typed hex string. */
sealed class FollowActionError(message: String) : Exception(message) {
    data class InvalidTarget(val got: String) :
        FollowActionError("invalid public key: $got")

    data object AutomaticRoutingUnavailable :
        FollowActionError("automatic author/outbox routing is not configured")

    data object SignedOut : FollowActionError("no current account is selected")
    data object EngineClosed : FollowActionError("the engine is closed")
    data class PublishRefused(val reason: String) : FollowActionError(reason)

    companion object {
        internal fun from(error: FfiFollowActionException): FollowActionError =
            when (error) {
                is FfiFollowActionException.InvalidTarget -> InvalidTarget(error.got)
                is FfiFollowActionException.AutomaticRoutingUnavailable ->
                    AutomaticRoutingUnavailable
                is FfiFollowActionException.SignedOut -> SignedOut
                is FfiFollowActionException.EngineClosed -> EngineClosed
                is FfiFollowActionException.PublishRefused -> PublishRefused(error.reason)
            }
    }
}

/** Observe whether the current account follows [target] through the
 * NMP-owned NIP-02 resource (mirrors `NMPEngine.observeFollowing`). This is
 * NMP's protocol projection, not an app-maintained boolean.
 *
 * Each element is the full current [FollowingSnapshot] -- latest-wins,
 * never a growing backlog: the engine's latest-state mailbox conflates
 * intermediate snapshots for a slow collector (#680), so the wrapper is a
 * thin pull loop over `NmpFollowStream.next()`. Demand teardown is
 * collection-scope-tied via `handle.cancel()` in a `finally`, identical
 * reasoning to `Query.kt`'s header. */
fun observeFollowing(engine: NmpEngineInterface, target: String): Flow<FollowingSnapshot> =
    flow {
        val handle = nmpRethrowing { engine.observeFollowing(target) }
        try {
            while (true) {
                val snapshot = nmpRethrowingAsync { handle.next() } ?: break
                emit(FollowingSnapshot.from(snapshot))
            }
        } finally {
            handle.cancel()
        }
    }

/** Ask NMP to follow [target] through the ordinary durable write and receipt
 * lifecycle (mirrors `NMPEngine.follow`). NMP immediately applies one durable
 * semantic operation to the best cached contact list, or to NIP-02's complete
 * empty list when no source exists, and reapplies it if a newer relay source
 * arrives. Either a truthful immediate [FollowActionError], or the same
 * [Receipt] every other write returns. */
fun NMPEngine.follow(target: String): Receipt = followReceipt { ffi.follow(target) }

/** The inverse of [follow], with the same durable operation and ordinary
 * receipt guarantees (mirrors `NMPEngine.unfollow`). */
fun NMPEngine.unfollow(target: String): Receipt = followReceipt { ffi.unfollow(target) }

private fun NMPEngine.followReceipt(action: () -> uniffi.nmp_ffi.NmpReceiptStream): Receipt =
    try {
        receiptFrom(action())
    } catch (error: FfiFollowActionException) {
        throw FollowActionError.from(error)
    }
