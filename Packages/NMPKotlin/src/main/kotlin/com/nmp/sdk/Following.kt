// NMP's optional NIP-02 following resource/action, in ergonomic Kotlin
// shape. Mirrors Following.swift's `NMPEngine.observeFollowing`/`follow`/
// `unfollow` PUBLIC semantics exactly: same states, same typed failures,
// same "the action returns immediately with a status stream" contract. No
// contact-list parsing, replacement composition, or readiness policy lives
// on this side of the FFI boundary -- this file only mirrors Rust-owned
// state and drains Rust-owned streams, exactly like `nmp-ffi/src/nip02.rs`'s
// own header comment says of the Rust side.
//
// SCOPE NOTE: Following.swift also defines `NMPFollowing`, a `@MainActor`
// `ObservableObject` that bundles `canToggle`/`offersAnotherAttempt`/
// `toggle()`/`performPrimaryAction()` local UI-state bookkeeping on top of
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
import uniffi.nmp_ffi.FfiFollowActionFailure
import uniffi.nmp_ffi.FfiFollowActionStatus
import uniffi.nmp_ffi.FfiFollowAvailability
import uniffi.nmp_ffi.FfiFollowRelationship
import uniffi.nmp_ffi.FfiFollowSnapshot
import uniffi.nmp_ffi.NmpEngineInterface
import uniffi.nmp_ffi.NmpFollowActionStream

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

/** Every typed way NMP's follow/unfollow action can end without changing
 * the relationship (`FfiFollowActionFailure` mirror). */
sealed class FollowActionFailure {
    data class InvalidTarget(val got: String) : FollowActionFailure()

    object SignedOut : FollowActionFailure()

    object EngineClosed : FollowActionFailure()

    object ReceiptUnavailable : FollowActionFailure()

    companion object {
        fun from(ffi: FfiFollowActionFailure): FollowActionFailure =
            when (ffi) {
                is FfiFollowActionFailure.InvalidTarget -> InvalidTarget(ffi.got)
                is FfiFollowActionFailure.SignedOut -> SignedOut
                is FfiFollowActionFailure.EngineClosed -> EngineClosed
                is FfiFollowActionFailure.ReceiptUnavailable -> ReceiptUnavailable
            }
    }
}

/** A thin projection of the ordinary receipt created by a typed
 * `follow`/`unfollow` action. Successful actions contain only canonical
 * [WriteFact] values; immediate typed refusal is the sole non-receipt case. */
sealed class FollowActionStatus {
    data class Receipt(val id: ULong, val status: WriteFact) : FollowActionStatus()

    data class Failed(val failure: FollowActionFailure) : FollowActionStatus()

    companion object {
        fun from(ffi: FfiFollowActionStatus): FollowActionStatus =
            when (ffi) {
                is FfiFollowActionStatus.Receipt ->
                    Receipt(ffi.receiptId, WriteFact.from(ffi.status))
                is FfiFollowActionStatus.Failed -> Failed(FollowActionFailure.from(ffi.failure))
            }
    }
}

/** A typed follow/unfollow action's ordinary receipt projection. The stable
 * receipt id arrives inside [FollowActionStatus.Receipt]. */
data class FollowAction(val status: Flow<FollowActionStatus>)

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

/** Shared pull loop for the ordinary receipt facts projected by a typed
 * `follow`/`unfollow` action. Teardown is collection-scope-tied via
 * `handle.cancel()`. */
private fun followActionFlow(open: () -> NmpFollowActionStream): Flow<FollowActionStatus> =
    flow {
        val handle = nmpRethrowing { open() }
        try {
            while (true) {
                val status = nmpRethrowingAsync { handle.next() } ?: break
                emit(FollowActionStatus.from(status))
            }
        } finally {
            handle.cancel()
        }
    }

/** Ask NMP to follow [target] (mirrors `NMPEngine.follow`). NMP immediately
 * applies one durable semantic operation to the best cached contact list, or
 * to NIP-02's complete empty list when no source exists. If a newer relay
 * source arrives, NMP reapplies the same operation while retaining the same
 * receipt. The caller only observes [FollowAction.status]. A missing automatic
 * route provider is refused before custody. An invalid [target] surfaces as
 * `FollowActionStatus.Failed(FollowActionFailure.InvalidTarget)` on the
 * stream, not as a synchronous exception). */
fun follow(engine: NmpEngineInterface, target: String): FollowAction =
    FollowAction(followActionFlow { engine.follow(target) })

/** The inverse of [follow], with the same durable semantic operation and
 * ordinary receipt guarantees (mirrors `NMPEngine.unfollow`). */
fun unfollow(engine: NmpEngineInterface, target: String): FollowAction =
    FollowAction(followActionFlow { engine.unfollow(target) })
