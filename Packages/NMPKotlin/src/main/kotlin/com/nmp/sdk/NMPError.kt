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
    data class AuthCapabilityRegistryFull(val limit: ULong) :
        NMPError("AUTH capability registry is full at $limit")
    object AuthCapabilityInstanceExhausted :
        NMPError("AUTH capability instance namespace exhausted")
    object NoActiveSigner : NMPError("the active account has no registered signer")
    data class InvalidSignRequest(val reason: String) : NMPError("invalid sign request: $reason")
    data class SignerUnavailable(val reason: String) : NMPError("signer unavailable: $reason")
    data class SignerRejected(val reason: String) : NMPError("signer rejected request: $reason")
    data class InvalidSignerOutput(val reason: String) :
        NMPError("signer returned invalid output: $reason")
    object ReceiptCorrelationIdExhausted :
        NMPError("receipt correlation id namespace exhausted")
    data class StoreOpenFailed(val reason: String) : NMPError("store open failed: $reason")
    /** #489: the configured `storePath` names a persistent store already owned
     * by this or another process. No second database owner and no partial
     * engine were created. */
    data class StoreAlreadyOpen(val path: String) :
        NMPError("persistent store is already open: $path")
    data class StoreResetFailed(val reason: String) : NMPError("store reset failed: $reason")
    data class StoreStillOpen(val path: String) : NMPError("persistent store is still open: $path")
    /** The engine could not be constructed (`NmpEngine` creation): a genuine
     * engine-start infrastructure failure. Never raised by an ordinary
     * operation (#704). */
    data class EngineStartFailed(val component: String, val reason: String) :
        NMPError("engine could not start ($component): $reason")
    /** A windowed `observe` could not open its canonical history projection
     * because the store degraded during setup. This is the case's sole
     * production meaning; relay connection/worker failure remains ordinary
     * acquisition evidence in the observation stream (#704). */
    data class ObservationUnavailable(val reason: String) :
        NMPError("observation could not be established: $reason")
    /** A second `next()`/`signed()` was awaited on an observation stream or
     * handle while a previous one was still in flight (#680). The streams are
     * single-consumer: await the next pull only after the previous one has
     * resolved. No frame is lost or duplicated -- only the offending call is
     * rejected. In practice the SDK's own `Flow`/`suspend` wrappers never
     * issue overlapping pulls, so this surfaces only when app code collects
     * one stream's `Flow` from two coroutines at once. */
    object ConcurrentNext : NMPError("a next() is already in flight on this single-consumer stream")
    /** A durable FIFO fact stream crossed its finite live-delivery bound
     * while the app was paused. Memory remains bounded and no missing fact is
     * claimed delivered; when non-null, reattach [receiptId] to replay. */
    data class FactStreamLagged(val receiptId: ULong?) :
        NMPError(
            receiptId?.let {
                "the finite live fact stream fell behind; reattach receipt $it to replay"
            } ?: "the finite live fact stream fell behind before a receipt was observable",
        )
    data class ReceiptReplayUnavailable(val receiptId: ULong) :
        NMPError("retained evidence for receipt $receiptId became unavailable during replay")
    data class InvalidSignature(val got: String) : NMPError("invalid signature: $got")
    object EngineClosed : NMPError("engine already shut down")
    /** A separately packaged native component was built against a different
     * core Rust object contract. Refused before an external object pointer
     * crosses the component boundary (#952). */
    data class NativeComponentMismatch(
        val component: String,
        val expectedCoreIdentity: String,
        val actualCoreIdentity: String,
    ) : NMPError(
        "native component $component requires core $expectedCoreIdentity, loaded $actualCoreIdentity",
    )
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

    /** A windowed `observe` declared a zero `initial` or `max` row count
     * (#485) -- an empty window could never deliver a row. */
    object WindowZeroRows : NMPError("window initial and max row counts must be non-zero")

    /** A windowed `observe` declared `initial > max` (#485) -- the window
     * would start above its own declared ceiling. */
    data class WindowInitialExceedsMax(val initial: ULong, val max: ULong) :
        NMPError("window initial $initial exceeds max $max")

    /** A windowed `observe` selection already declares a NIP-01 `limit`
     * (#485) -- the window IS the bound; carrying a second bound in the
     * selection would let the two silently fight. */
    object WindowSelectionHasLimit :
        NMPError("windowed selection must not also declare a limit")

    /** A windowed `observe` was given a live query that already declares an
     * aggregate result limit (#1108) -- the window and the aggregate bound
     * would be two competing owners of the merged row count. */
    object WindowAggregateResultLimit :
        NMPError("a windowed observation must not also declare an aggregate result limit")

    /** A live query was declared with no demand branches at all (#1108). */
    object EmptyQueryUnion :
        NMPError("a live query must declare at least one demand branch")

    /** A live query declared an aggregate result limit of zero (#1108): a
     * query that may never contain a row is not a bound. */
    object AggregateResultLimitZero :
        NMPError("an aggregate result limit of zero can never contain a row")

    /** A nested live-query branch carried its own aggregate result limit
     * (#1108). Branches flatten into one canonical set, so an inner bound has
     * no surviving scope and accepting it would silently discard it. */
    object NestedAggregateResultLimit :
        NMPError("a nested live-query branch must not declare its own aggregate result limit")

    /** A live query declared more branches than the supported hard ceiling
     * (#1108). The whole declaration is refused; no subset is installed. */
    data class TooManyQueryBranches(val requested: ULong, val maximum: ULong) :
        NMPError("a live query supports at most $maximum demand branches; $requested were declared")

    data class RelayInformationUnavailable(val kind: RelayInformationErrorKind) :
        NMPError("relay information unavailable: ${kind.describe()}")

    /** #591: [WriteIntent.correlation]/`reattachReceipt`'s correlation
     * overload was given a token that failed the bounded/non-empty
     * validation. */
    data class InvalidCorrelationToken(val got: String, val reason: String) :
        NMPError("invalid correlation token $got: $reason")

    /** #572: an `Nip73Target` failed its constructor validation (an empty
     * `I`/`K` cell). */
    data class InvalidNip73Target(val reason: String) :
        NMPError("invalid NIP-73 target: $reason")

    /** #973: a composer returned a compare-and-swap replaceable edit, which
     * has no wire form on purpose -- a replaceable precondition crosses this
     * boundary only inside the semantic method that owns it
     * (`follow`/`unfollow`), never as a payload a native caller could
     * reassemble without the guard. */
    object ReplaceableEditHasNoWireForm :
        NMPError(
            "a replaceable edit crosses this boundary only inside the semantic method that owns " +
                "its precondition, never as a payload",
        )

    /** #1033: `NMPRelayScope.on`/`FfiRelayScope.on` was given an empty relay
     * set -- a group must be hosted somewhere. */
    object EmptyRelayScope :
        NMPError("RelayScope.on requires a nonempty relay set -- a group must be hosted somewhere")

    /** #1033: an event builder handed to `NMPGroup.publish` already carried
     * its own `h` tag. The retained group id is the sole semantic source of
     * that tag; a caller-supplied one is refused before any write reaches
     * the door. */
    object GroupCallerSuppliedContext :
        NMPError(
            "a group write must not carry its own h tag; the group's retained id is the sole " +
                "source of that tag",
        )

    /** #1033: a read selection handed to `NMPGroup.read` already constrained
     * `#h`. The retained group id is the sole semantic source of that row. */
    object GroupCallerSuppliedContextConstraint :
        NMPError(
            "a group read selection must not already constrain #h; the group's retained id is " +
                "the sole source of that row",
        )

    /** #1033: a read selection handed to `NMPGroup.read` already declared a
     * `since`/`until`/`limit` timeline bound the group door itself owns. */
    object GroupCallerSuppliedTimeline :
        NMPError(
            "a group read selection must not already declare since/until/limit; the group door " +
                "owns that bound",
        )

    /** #1033: `NMPGroup.validateContext`/`publishSigned` was given an event
     * carrying no `h` tag naming any group at all. */
    data class GroupContextMissing(val expected: String) :
        NMPError("event carries no h tag; expected group $expected")

    /** #1033: an already-signed event's `h` tag names a different group than
     * the one it was handed to. */
    data class GroupContextMismatched(val found: String, val expected: String) :
        NMPError("event's h tag $found does not match expected group $expected")

    /** #1033: an already-signed event carried more than one distinct `h`
     * tag, so which group it belongs to is ambiguous. */
    data class GroupContextAmbiguous(val expected: String) :
        NMPError("event carries more than one distinct h tag; expected exactly group $expected")

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
                is FfiException.AuthCapabilityRegistryFull -> AuthCapabilityRegistryFull(ffi.limit)
                is FfiException.AuthCapabilityInstanceExhausted -> AuthCapabilityInstanceExhausted
                is FfiException.NoActiveSigner -> NoActiveSigner
                is FfiException.InvalidSignRequest -> InvalidSignRequest(ffi.reason)
                is FfiException.ReceiptCorrelationIdExhausted -> ReceiptCorrelationIdExhausted
                is FfiException.StoreOpenFailed -> StoreOpenFailed(ffi.reason)
                is FfiException.StoreAlreadyOpen -> StoreAlreadyOpen(ffi.path)
                is FfiException.StoreResetFailed -> StoreResetFailed(ffi.reason)
                is FfiException.StoreStillOpen -> StoreStillOpen(ffi.path)
                is FfiException.EngineStartFailed -> EngineStartFailed(ffi.component, ffi.reason)
                is FfiException.ObservationUnavailable -> ObservationUnavailable(ffi.reason)
                is FfiException.ConcurrentNext -> ConcurrentNext
                is FfiException.FactStreamLagged -> FactStreamLagged(ffi.receiptId)
                is FfiException.ReceiptReplayUnavailable ->
                    ReceiptReplayUnavailable(ffi.receiptId)
                is FfiException.InvalidSignature -> InvalidSignature(ffi.got)
                is FfiException.EngineClosed -> EngineClosed
                is FfiException.InvalidNostrEntity -> InvalidNostrEntity(ffi.reason)
                is FfiException.NostrEntitySecretKeyRejected -> NostrEntitySecretKeyRejected
                is FfiException.AuthorOutboxesRequiresBoundAuthors -> AuthorOutboxesRequiresBoundAuthors
                is FfiException.EmptyPinnedRelaySet -> EmptyPinnedRelaySet
                is FfiException.WindowZeroRows -> WindowZeroRows
                is FfiException.WindowInitialExceedsMax ->
                    WindowInitialExceedsMax(ffi.initial, ffi.max)
                is FfiException.WindowSelectionHasLimit -> WindowSelectionHasLimit
                is FfiException.WindowAggregateResultLimit -> WindowAggregateResultLimit
                is FfiException.EmptyQueryUnion -> EmptyQueryUnion
                is FfiException.AggregateResultLimitZero -> AggregateResultLimitZero
                is FfiException.NestedAggregateResultLimit -> NestedAggregateResultLimit
                is FfiException.TooManyQueryBranches ->
                    TooManyQueryBranches(ffi.requested, ffi.maximum)
                is FfiException.RelayInformationUnavailable ->
                    RelayInformationUnavailable(RelayInformationErrorKind.from(ffi.kind))
                is FfiException.InvalidCorrelationToken ->
                    InvalidCorrelationToken(ffi.got, ffi.reason)
                is FfiException.InvalidNip73Target -> InvalidNip73Target(ffi.reason)
                is FfiException.ReplaceableEditHasNoWireForm -> ReplaceableEditHasNoWireForm
                is FfiException.EmptyRelayScope -> EmptyRelayScope
                is FfiException.GroupCallerSuppliedContext -> GroupCallerSuppliedContext
                is FfiException.GroupCallerSuppliedContextConstraint ->
                    GroupCallerSuppliedContextConstraint
                is FfiException.GroupCallerSuppliedTimeline -> GroupCallerSuppliedTimeline
                is FfiException.GroupContextMissing -> GroupContextMissing(ffi.expected)
                is FfiException.GroupContextMismatched ->
                    GroupContextMismatched(ffi.found, ffi.expected)
                is FfiException.GroupContextAmbiguous -> GroupContextAmbiguous(ffi.expected)
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
