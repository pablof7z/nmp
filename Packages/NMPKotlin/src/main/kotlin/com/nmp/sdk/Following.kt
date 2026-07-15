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

import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.channels.awaitClose
import kotlinx.coroutines.channels.trySendBlocking
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.callbackFlow
import kotlinx.coroutines.flow.conflate
import kotlinx.coroutines.flow.receiveAsFlow
import uniffi.nmp_ffi.FfiFollowActionFailure
import uniffi.nmp_ffi.FfiFollowActionStatus
import uniffi.nmp_ffi.FfiFollowAvailability
import uniffi.nmp_ffi.FfiFollowRelationship
import uniffi.nmp_ffi.FfiFollowSnapshot
import uniffi.nmp_ffi.FollowActionObserver
import uniffi.nmp_ffi.FollowObserver
import uniffi.nmp_ffi.NmpEngineInterface

/** The active account's relationship to a `target` pubkey, as NMP's own
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

/** Whether NMP's NIP-02 action can safely compose a whole-list replacement
 * from the current source-scoped snapshot (`FfiFollowAvailability` mirror).
 * `Ready` is explicitly about every source in the current plan; it is not a
 * claim that Nostr is globally complete. */
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

/** One pushed state of the active account's relationship to `target`
 * (`FfiFollowSnapshot` mirror). Delivered by `NMPEngine.observeFollowing`,
 * pushed reactively, never polled. */
data class FollowingSnapshot(
    val activePubkey: String?,
    val target: String,
    val relationship: FollowRelationship,
    val availability: FollowAvailability,
    val baseEventId: String?,
) {
    companion object {
        fun from(ffi: FfiFollowSnapshot): FollowingSnapshot =
            FollowingSnapshot(
                activePubkey = ffi.activePubkey,
                target = ffi.target,
                relationship = FollowRelationship.from(ffi.relationship),
                availability = FollowAvailability.from(ffi.availability),
                baseEventId = ffi.baseEventId,
            )

        /** The pre-acquisition placeholder a caller may render before the
         * first real snapshot arrives (mirrors
         * `NMPFollowingSnapshot.initial(target:)`). Kotlin's `callbackFlow`
         * -- unlike Swift's synchronously-constructed `struct` -- has no
         * value at all until the flow is collected and the engine has
         * pushed one, so a caller that wants an immediate placeholder value
         * (e.g. to seed a `MutableStateFlow` before `collect` starts) uses
         * this explicitly; `observeFollowing` itself only ever emits real,
         * engine-sourced snapshots. */
        fun initial(target: String): FollowingSnapshot =
            FollowingSnapshot(
                activePubkey = null,
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

    object AccountChanged : FollowActionFailure()

    object AcquisitionTimedOut : FollowActionFailure()

    object NoContactList : FollowActionFailure()

    object CachedOnly : FollowActionFailure()

    object SourceUnavailable : FollowActionFailure()

    object BaseHasWrongAuthor : FollowActionFailure()

    object BaseHasWrongKind : FollowActionFailure()

    object TimestampExhausted : FollowActionFailure()

    object InvalidGeneratedTag : FollowActionFailure()

    object EngineClosed : FollowActionFailure()

    object ReceiptUnavailable : FollowActionFailure()

    data class ThreadUnavailable(val component: String, val reason: String) : FollowActionFailure()

    data class ExecutorSaturated(val component: String, val capacity: ULong) : FollowActionFailure()

    companion object {
        fun from(ffi: FfiFollowActionFailure): FollowActionFailure =
            when (ffi) {
                is FfiFollowActionFailure.InvalidTarget -> InvalidTarget(ffi.got)
                is FfiFollowActionFailure.SignedOut -> SignedOut
                is FfiFollowActionFailure.AccountChanged -> AccountChanged
                is FfiFollowActionFailure.AcquisitionTimedOut -> AcquisitionTimedOut
                is FfiFollowActionFailure.NoContactList -> NoContactList
                is FfiFollowActionFailure.CachedOnly -> CachedOnly
                is FfiFollowActionFailure.SourceUnavailable -> SourceUnavailable
                is FfiFollowActionFailure.BaseHasWrongAuthor -> BaseHasWrongAuthor
                is FfiFollowActionFailure.BaseHasWrongKind -> BaseHasWrongKind
                is FfiFollowActionFailure.TimestampExhausted -> TimestampExhausted
                is FfiFollowActionFailure.InvalidGeneratedTag -> InvalidGeneratedTag
                is FfiFollowActionFailure.EngineClosed -> EngineClosed
                is FfiFollowActionFailure.ReceiptUnavailable -> ReceiptUnavailable
                is FfiFollowActionFailure.ThreadUnavailable -> ThreadUnavailable(ffi.component, ffi.reason)
                is FfiFollowActionFailure.ExecutorSaturated ->
                    ExecutorSaturated(ffi.component, ffi.capacity)
            }
    }
}

/** One pushed state of a `follow`/`unfollow` action's outcome
 * (`FfiFollowActionStatus` mirror): acquisition, no-op, atomic conflict
 * (folded into the `Receipt` case's own `WriteStatus.ReplaceableConflict`),
 * signing, routing, and relay receipt states all arrive through this one
 * typed stream. */
sealed class FollowActionStatus {
    object Acquiring : FollowActionStatus()

    data class NoChange(val following: Boolean) : FollowActionStatus()

    data class Receipt(val id: ULong, val status: WriteStatus) : FollowActionStatus()

    data class Failed(val failure: FollowActionFailure) : FollowActionStatus()

    companion object {
        fun from(ffi: FfiFollowActionStatus): FollowActionStatus =
            when (ffi) {
                is FfiFollowActionStatus.Acquiring -> Acquiring
                is FfiFollowActionStatus.NoChange -> NoChange(ffi.following)
                is FfiFollowActionStatus.Receipt ->
                    Receipt(ffi.receiptId, WriteStatus.from(ffi.status))
                is FfiFollowActionStatus.Failed -> Failed(FollowActionFailure.from(ffi.failure))
            }
    }
}

/** A started `follow`/`unfollow` action's identity: just its status stream
 * (mirrors `NMPFollowAction`). Unlike [Receipt], there is no separate stable
 * id here -- the receipt id, once acquisition succeeds, arrives inside
 * [FollowActionStatus.Receipt] itself. */
data class FollowAction(val status: Flow<FollowActionStatus>)

/** Observe whether the active account follows [target] through the
 * NMP-owned NIP-02 resource (mirrors `NMPEngine.observeFollowing`). This is
 * NMP's protocol projection, not an app-maintained boolean.
 *
 * Each element is the full current [FollowingSnapshot] -- latest-wins,
 * never a growing backlog: `.conflate()` gives the same "latest-wins"
 * delivery discipline as Swift's `AsyncStream(bufferingPolicy:
 * .bufferingNewest(1))`, same as `observeQuery`'s own finding in
 * `Query.kt`. Demand teardown is collection-scope-tied via `awaitClose`,
 * identical reasoning to that file's header. */
fun observeFollowing(engine: NmpEngineInterface, target: String): Flow<FollowingSnapshot> =
    callbackFlow {
        val observer =
            object : FollowObserver {
                override fun onSnapshot(snapshot: FfiFollowSnapshot) {
                    trySendBlocking(FollowingSnapshot.from(snapshot))
                }

                override fun onClosed() {
                    close()
                }
            }

        val handle = nmpRethrowing { engine.observeFollowing(target, observer) }

        awaitClose { handle.cancel() }
    }.conflate()

/** Shared bridge for the `follow`/`unfollow` action status stream. Unlike
 * [observeFollowing]'s conflated relationship stream, this mirrors
 * `Receipt.kt`'s `ReceiptBridge` shape instead: every status matters (the
 * caller is watching a one-shot action run to completion, not a live
 * projection it may fall behind on), so this is an unbounded `Channel`, the
 * same delivery discipline as Swift's un-throttled `AsyncStream` for
 * `NMPFollowAction.status`. */
private class FollowActionBridge {
    val channel = Channel<FollowActionStatus>(Channel.UNLIMITED)
    val observer =
        object : FollowActionObserver {
            override fun onStatus(status: FfiFollowActionStatus) {
                channel.trySendBlocking(FollowActionStatus.from(status))
            }

            override fun onClosed() {
                channel.close()
            }
        }
}

/** Ask NMP to follow [target] (mirrors `NMPEngine.follow`). This is the
 * complete NIP-02 action: it waits for the module's source-evidence policy,
 * preserves the exact kind:3 base, atomically guards that base, signs,
 * routes, and streams the durable receipt. The caller owns none of those
 * steps -- it only observes [FollowAction.status]. Returns immediately;
 * never throws (an invalid [target] surfaces as
 * `FollowActionStatus.Failed(FollowActionFailure.InvalidTarget)` on the
 * stream, not as a synchronous exception). */
fun follow(engine: NmpEngineInterface, target: String): FollowAction {
    val bridge = FollowActionBridge()
    engine.follow(target, bridge.observer)
    return FollowAction(bridge.channel.receiveAsFlow())
}

/** The inverse of [follow], with the same acquisition, compare-and-swap,
 * signer, routing, and receipt guarantees (mirrors `NMPEngine.unfollow`). */
fun unfollow(engine: NmpEngineInterface, target: String): FollowAction {
    val bridge = FollowActionBridge()
    engine.unfollow(target, bridge.observer)
    return FollowAction(bridge.channel.receiveAsFlow())
}
