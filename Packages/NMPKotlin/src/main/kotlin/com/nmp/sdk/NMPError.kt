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
 * only this one). Because that instruction can never resolve, it surfaces as
 * [NMPError.PublishRefused] from `publish` itself rather than as a fact on a
 * receipt stream nothing will ever add to.
 * Receipt-correlation exhaustion is synchronous because no truthful
 * `Receipt` or status flow can be created without an identity.
 *
 */
sealed class NMPError(message: String) : Exception(message) {
    data class NonIndexableFilterTag(val got: String) :
        NMPError("not indexable as a filter key: $got")
    data class InvalidPublicKey(val got: String) : NMPError("invalid public key: $got")
    object InvalidPublicKeyBytes : NMPError("invalid decoded public key")
    object InvalidPrivateKeyBytes : NMPError("invalid decoded private key")
    object SessionMalformedPayload : NMPError("malformed session payload")
    data class SessionUnsupportedVersion(val found: UShort) :
        NMPError("unsupported session payload version $found")
    data class SessionUnsupportedProvider(val id: String) :
        NMPError("unsupported session provider $id")
    data class SessionUnsupportedProviderVersion(val provider: String, val found: UShort) :
        NMPError("unsupported $provider provider version $found")
    object SessionDuplicateAccount : NMPError("duplicate session account")
    object SessionCurrentAccountMissing : NMPError("current session account is missing")
    data class SessionProviderPayloadInvalid(val provider: String) :
        NMPError("invalid $provider provider payload")
    object SessionProviderPublicKeyMismatch :
        NMPError("session provider public key mismatch")
    object SessionAccountNotFound : NMPError("session account does not exist")
    data class InvalidEventId(val got: String) : NMPError("invalid event id: $got")
    data class InvalidRelayUrl(val got: String) : NMPError("invalid relay url: $got")
    // nmp-native:if nip65
    object OutboxRoutingIndexersEmpty :
        NMPError("outbox routing requires at least one app-owned indexer")
    object AutomaticRoutingUnavailable :
        NMPError("automatic routing is unavailable; configure outbox routing indexers")
    // nmp-native:endif
    data class InvalidTag(val got: List<String>) : NMPError("invalid tag: $got")
    data class InvalidSigner(val reason: String) : NMPError("invalid signer: $reason")
    data class AuthCapabilityRegistryFull(val limit: ULong) :
        NMPError("AUTH capability registry is full at $limit")
    object AuthCapabilityInstanceExhausted :
        NMPError("AUTH capability instance namespace exhausted")
    object NoCurrentSigningProvider : NMPError("the current account has no signing provider")
    data class InvalidSignRequest(val reason: String) : NMPError("invalid sign request: $reason")
    data class SignerUnavailable(val reason: String) : NMPError("signer unavailable: $reason")
    data class SignerRejected(val reason: String) : NMPError("signer rejected request: $reason")
    data class InvalidSignerOutput(val reason: String) :
        NMPError("signer returned invalid output: $reason")
    /** `publish` refused the call outright: either NMP could not write
     * anything down, or the instruction could not resolve (no current account,
     * a signature that does not verify, an explicit identity contradicting a
     * signed payload's author, a reserved kind, an empty explicit route).
     * Nothing durable exists and there is no queue entry to inspect.
     *
     * Everything else takes CUSTODY and fails in the queue where you can see
     * it -- including a stale replaceable base, which succeeds here and
     * arrives as [WriteOutcome.Refused]. */
    data class PublishRefused(val reason: String) : NMPError(reason)
    /** The configured `storePath` pointed at a file the on-disk store could
     * not open: damaged bytes, a refused lock, an unresolvable path, an I/O
     * failure.
     *
     * A positive claim, not a catch-all (#920): **deleting the store is not
     * the recovery for this.** The one refusal a fresh store does fix is
     * [StoreUnsupportedSchema], and it never arrives here. */
    data class StoreOpenFailed(val reason: String) : NMPError("store open failed: $reason")
    /** #489: the configured `storePath` names a persistent store already owned
     * by this or another process. No second database owner and no partial
     * engine were created. */
    data class StoreAlreadyOpen(val path: String) :
        NMPError("persistent store is already open: $path")
    /** #867/#920: the configured `storePath` holds durable bytes that are not
     * the one schema epoch this build supports. Nothing was migrated, adopted,
     * drained, or reset, and no engine was constructed.
     *
     * The only response that lets this build run is to close every owner,
     * delete the store, and create a fresh one -- your call, through the
     * separate destructive reset. The relay-backed read cache is reacquirable;
     * the publish queue is not, so accepted but unpublished writes and their
     * receipts, correlation tokens, route revisions, and attempt evidence go
     * with it.
     *
     * [found] is `null` when the store carries no marker this build can read,
     * which includes a marker written at an address a superseded epoch owned.
     * `null` means "not this epoch", never "no data". */
    data class StoreUnsupportedSchema(
        val path: String,
        val expected: ULong,
        val found: ULong?,
    ) : NMPError(
        (
            found?.let {
                "persistent store $path is schema epoch $it, not the one supported epoch $expected"
            }
                ?: "persistent store $path carries no readable schema marker and is not the one " +
                "supported epoch $expected"
            ) +
            "; it was not migrated, adopted, drained, or reset; discard and recreate this store " +
            "to continue; NMP can reacquire the relay-backed read cache, but the publish queue " +
            "state (accepted but unpublished writes, receipts, correlation tokens, route " +
            "revisions, and attempt evidence) will be permanently lost",
    )
    data class StoreResetFailed(val reason: String) : NMPError("store reset failed: $reason")
    data class StoreStillOpen(val path: String) : NMPError("persistent store is still open: $path")
    /** The engine could not be constructed (`NmpEngine` creation): a genuine
     * engine-start infrastructure failure. Never raised by an ordinary
     * operation (#704). */
    data class EngineStartFailed(val component: String, val reason: String) :
        NMPError("engine could not start ($component): $reason")
    /** Construction named a store that retains a replaceable operation whose
     * compiled program/format is absent from this engine. No engine started
     * and the store was not mutated.
     *
     * [programHex] and [formatHex] are the two 16-byte compiled-capability
     * identifiers as canonical lowercase hex (32 characters each). They are
     * opaque identities to compare, key, and show -- never a public key or an
     * event id, so nothing here is bech32-able. The FFI hands them over as raw
     * `ByteArray`s, which are mutable and compare by reference; hex is what
     * makes this error the value it claims to be, so two errors naming the
     * same missing capability are `==`, hash alike, and cannot be edited by
     * whoever received the first one. */
    data class MissingReplaceableCapability(val programHex: String, val formatHex: String) :
        NMPError("store retains replaceable operations for a missing compiled capability")
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
    data class ReceiptClosedWithoutOutcome(val receiptId: ULong) :
        NMPError("receipt $receiptId closed before its terminal outcome")
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

    // nmp-native:if nip22
    /** #572/#1258: an `Nip73` failed its constructor validation (an empty
     * `I`/`K` cell, or a `Url` that is not an absolute URL and therefore
     * cannot be normalised). */
    data class InvalidNip73(val reason: String) :
        NMPError("invalid NIP-73 external content id: $reason")
    // nmp-native:endif

    // nmp-native:if nip25
    /** #155: a [Reaction.Emoji] said something the caller did not mean -- the
     * empty string, which NIP-25 reads as a like, or a NIP-30 `:shortcode:`,
     * which needs a companion `emoji` row this door does not write. */
    data class InvalidReaction(val reason: String) :
        NMPError("invalid reaction: $reason")
    // nmp-native:endif

    // nmp-native:if nip22
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
    // nmp-native:endif

    /** #1437: registered replaceable operations are capability-owned internal
     * write payloads. They cannot be projected as a standalone native payload
     * without losing the registered materializer that gives the bytes their
     * meaning. */
    object ReplaceableOperationHasNoWireForm :
        NMPError("a registered replaceable operation has no standalone FFI payload")

    // nmp-native:if nip29
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

    /** #1281: `NMPRelayScope.groups` was given no group id at all. An event
     * with no `h` row is not in a group, so there is nothing to
     * contextualize and no honest route to mint. */
    object EmptyGroupSet :
        NMPError(
            "a group write must name at least one group; an event with no h row is not in a " +
                "group at all",
        )

    /** #1033/#1281: `NMPGroup.validateContext` was given an
     * event carrying no `h` tag naming any group at all. `expected` is the
     * whole set the door was asked for -- one id for an [NMPGroup], several
     * for an [NMPGroups]. */
    data class GroupContextMissing(val expected: List<String>) :
        NMPError("event carries no h tag; expected groups $expected")

    /** #1033/#1281: an already-signed event's `h` tags name a different SET
     * of groups than the one it was handed to -- too few, too many, or the
     * wrong ones. */
    data class GroupContextMismatched(val found: List<String>, val expected: List<String>) :
        NMPError("event's h tags name groups $found, expected groups $expected")

    /** #1281: an already-signed event names the right groups but repeats one
     * of them in a second `h` row, which is not a row the door would mint. */
    data class GroupContextRepeated(val repeated: List<String>) :
        NMPError("event names groups $repeated in more than one h row")

    /** #1245: a group content read named one of NIP-29's own relay-signed
     * group records. Those identify themselves a different way, so an
     * `h`-scoped read of them can only ever match nothing -- read them through
     * `NMPGroup.observeRecords` instead. */
    data class GroupRecordsNotContextScoped(val kinds: List<UShort>) :
        NMPError(
            "kinds $kinds are NIP-29's own relay-signed group records: they key themselves by " +
                "d, never by h, so no such event could ever match a group content read; " +
                "observe the group's records instead",
        )

    /** #1233: a records observation named none of the three records, which
     * would deliver a permanently empty state. */
    object GroupNoRecordSelected :
        NMPError(
            "a group records observation must name at least one of the three relay-signed records",
        )

    object GroupUserBatchEmpty :
        NMPError("a NIP-29 user operation must name at least one user")

    data class GroupUserBatchConflictingRoles(val pubkey: String) :
        NMPError("NIP-29 user operation names $pubkey with conflicting roles")

    /** #1252: a selection handed to `NMPGroupIds.whoseRecordMatches` named no
     * kind. It is evaluated with NIP-29's own pin, so it would match every
     * event the group's host holds. */
    object GroupIdSelectionNamesNoKind :
        NMPError(
            "a group-record selection must name at least one of NIP-29's three relay-signed " +
                "group record kinds",
        )

    /** #1252: a selection handed to `NMPGroupIds.whoseRecordMatches` named a
     * kind that is not one of NIP-29's three relay-signed group records. That
     * leaf is evaluated at the group's host, which is not authoritative for
     * anything else, so the read would silently under-resolve. Ids that come
     * from the app's OWN data go through `NMPGroupIds.anyOf` as a derived
     * binding carrying its own authority. */
    data class GroupIdSelectionNotAGroupRecordKind(val kind: UShort) :
        NMPError(
            "kind $kind is not one of NIP-29's three relay-signed group records; a group host " +
                "is not authoritative for it",
        )
    // nmp-native:endif

    companion object {
        fun from(ffi: FfiException): NMPError =
            when (ffi) {
                is FfiException.NonIndexableFilterTag -> NonIndexableFilterTag(ffi.got)
                is FfiException.InvalidPublicKey -> InvalidPublicKey(ffi.got)
                is FfiException.InvalidPublicKeyBytes -> InvalidPublicKeyBytes
                is FfiException.InvalidPrivateKeyBytes -> InvalidPrivateKeyBytes
                is FfiException.SessionMalformedPayload -> SessionMalformedPayload
                is FfiException.SessionUnsupportedVersion ->
                    SessionUnsupportedVersion(ffi.found)
                is FfiException.SessionUnsupportedProvider ->
                    SessionUnsupportedProvider(ffi.id)
                is FfiException.SessionUnsupportedProviderVersion ->
                    SessionUnsupportedProviderVersion(ffi.provider, ffi.found)
                is FfiException.SessionDuplicateAccount -> SessionDuplicateAccount
                is FfiException.SessionCurrentAccountMissing -> SessionCurrentAccountMissing
                is FfiException.SessionProviderPayloadInvalid ->
                    SessionProviderPayloadInvalid(ffi.provider)
                is FfiException.SessionProviderPublicKeyMismatch ->
                    SessionProviderPublicKeyMismatch
                is FfiException.SessionAccountNotFound -> SessionAccountNotFound
                is FfiException.InvalidEventId -> InvalidEventId(ffi.got)
                is FfiException.InvalidRelayUrl -> InvalidRelayUrl(ffi.got)
                // nmp-native:if nip65
                is FfiException.OutboxRoutingIndexersEmpty -> OutboxRoutingIndexersEmpty
                is FfiException.AutomaticRoutingUnavailable -> AutomaticRoutingUnavailable
                // nmp-native:endif
                is FfiException.InvalidTag -> InvalidTag(ffi.got)
                is FfiException.InvalidSigner -> InvalidSigner(ffi.reason)
                is FfiException.AuthCapabilityRegistryFull -> AuthCapabilityRegistryFull(ffi.limit)
                is FfiException.AuthCapabilityInstanceExhausted -> AuthCapabilityInstanceExhausted
                is FfiException.NoCurrentSigningProvider -> NoCurrentSigningProvider
                is FfiException.InvalidSignRequest -> InvalidSignRequest(ffi.reason)
                is FfiException.PublishRefused -> PublishRefused(ffi.reason)
                is FfiException.StoreOpenFailed -> StoreOpenFailed(ffi.reason)
                is FfiException.StoreAlreadyOpen -> StoreAlreadyOpen(ffi.path)
                is FfiException.StoreUnsupportedSchema ->
                    StoreUnsupportedSchema(ffi.path, ffi.expected, ffi.found)
                is FfiException.StoreResetFailed -> StoreResetFailed(ffi.reason)
                is FfiException.StoreStillOpen -> StoreStillOpen(ffi.path)
                is FfiException.EngineStartFailed -> EngineStartFailed(ffi.component, ffi.reason)
                is FfiException.MissingReplaceableCapability ->
                    MissingReplaceableCapability(
                        canonicalLowercaseHex(ffi.program),
                        canonicalLowercaseHex(ffi.format),
                    )
                is FfiException.ObservationUnavailable -> ObservationUnavailable(ffi.reason)
                is FfiException.ConcurrentNext -> ConcurrentNext
                is FfiException.FactStreamLagged -> FactStreamLagged(ffi.receiptId)
                is FfiException.ReceiptReplayUnavailable ->
                    ReceiptReplayUnavailable(ffi.receiptId)
                is FfiException.ReceiptClosedWithoutOutcome ->
                    ReceiptClosedWithoutOutcome(ffi.receiptId)
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
                // nmp-native:if nip22
                is FfiException.InvalidNip73 -> InvalidNip73(ffi.reason)
                // nmp-native:endif
                // nmp-native:if nip25
                is FfiException.InvalidReaction -> InvalidReaction(ffi.reason)
                // nmp-native:endif
                // nmp-native:if nip22
                is FfiException.ReplaceableEditHasNoWireForm -> ReplaceableEditHasNoWireForm
                // nmp-native:endif
                is FfiException.ReplaceableOperationHasNoWireForm ->
                    ReplaceableOperationHasNoWireForm
                // nmp-native:if nip29
                is FfiException.EmptyRelayScope -> EmptyRelayScope
                is FfiException.GroupCallerSuppliedContext -> GroupCallerSuppliedContext
                is FfiException.GroupCallerSuppliedContextConstraint ->
                    GroupCallerSuppliedContextConstraint
                is FfiException.GroupCallerSuppliedTimeline -> GroupCallerSuppliedTimeline
                is FfiException.GroupContextMissing -> GroupContextMissing(ffi.expected)
                is FfiException.GroupContextMismatched ->
                    GroupContextMismatched(ffi.found, ffi.expected)
                is FfiException.GroupContextRepeated -> GroupContextRepeated(ffi.repeated)
                is FfiException.EmptyGroupSet -> EmptyGroupSet
                is FfiException.GroupRecordsNotContextScoped ->
                    GroupRecordsNotContextScoped(ffi.kinds)
                is FfiException.GroupNoRecordSelected -> GroupNoRecordSelected
                is FfiException.GroupUserBatchEmpty -> GroupUserBatchEmpty
                is FfiException.GroupUserBatchConflictingRoles ->
                    GroupUserBatchConflictingRoles(ffi.pubkey)
                is FfiException.GroupIdSelectionNamesNoKind -> GroupIdSelectionNamesNoKind
                is FfiException.GroupIdSelectionNotAGroupRecordKind ->
                    GroupIdSelectionNotAGroupRecordKind(ffi.kind)
                // nmp-native:endif
            }
    }
}

/** Canonical lowercase hex for an opaque byte identity the FFI hands over as
 * raw bytes. The exact spelling `NMPError.swift`'s `canonicalLowercaseHex`
 * produces for the same bytes: one identity, one rendering, both SDKs. */
private fun canonicalLowercaseHex(bytes: ByteArray): String =
    bytes.joinToString(separator = "") { byte -> "%02x".format(byte.toInt() and 0xff) }

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
