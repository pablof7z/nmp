// `NMPEngine` -- the ONE construction call the M4 kill test requires (plan
// §7): everything past `init` is a method call on this object, never a
// second container/provider the app must adopt. Kotlin/JVM mirror of
// Engine.swift.

package com.nmp.sdk

import kotlinx.coroutines.flow.Flow
import uniffi.nmp_ffi.NmpEngine
import uniffi.nmp_ffi.NmpEngineConfig
// nmp-native:if nip65
import uniffi.nmp_ffi.FfiOutboxRoutingConfig
// nmp-native:endif
import uniffi.nmp_ffi.resetPersistentStore as ffiResetPersistentStore

// nmp-native:if nip65
/** Runtime inputs for outbox routing selected by this app. NMP never supplies
 * hidden indexers. */
data class OutboxRoutingConfig(val indexers: List<String>)
// nmp-native:endif

/** Construction config for `NMPEngine`. Build-time feature selection controls
 * which fields exist; runtime relay values remain app-owned inputs. */
data class NMPConfig(
    /** `null` -> in-memory store (nothing survives a restart). A path ->
     * a persistent store reopened at that path across launches. */
    val storePath: String? = null,
    /** Operator app relay set (`Lane::OperatorApp`) -- every kind, every
     * author, always, additive. Default empty. */
    val appRelays: List<String> = emptyList(),
    /** Operator fallback relay set (`Lane::OperatorFallback`) -- tops up authors
     * under the 2-relay-min, suppressed when `appRelays` is non-empty.
     * Default empty. */
    val fallbackRelays: List<String> = emptyList(),
    // nmp-native:if nip65
    /** Optional outbox routing. `null` constructs an explicit-routing-only
     * engine. A configured capability must name at least one app-owned
     * indexer or construction throws. */
    val outboxRouting: OutboxRoutingConfig? = null,
    // nmp-native:endif
    /** The one whole-engine relay ceiling. It bounds the complete compiled
     * demand and simultaneous physical transport workers with the same
     * effective value. Access contexts never share a socket; competing read
     * and write contexts for the same admitted relay time-share its slot and
     * the read is restored afterward, so apps do not multiply this value per
     * context. Legacy zero is normalized to the finite default, never
     * uncapped. */
    val maxRelays: UInt = 10u,
    /** Finite shared ceiling for live signer and AUTH-policy registrations. Zero admits none. */
    val maxAuthCapabilities: UInt = 64u,
) {
    fun toFfi(): NmpEngineConfig =
        NmpEngineConfig(
            storePath = storePath,
            appRelays = appRelays,
            fallbackRelays = fallbackRelays,
            // nmp-native:if nip65
            outboxRouting = outboxRouting?.let { FfiOutboxRoutingConfig(it.indexers) },
            // nmp-native:endif
            maxRelays = maxRelays,
            maxAuthCapabilities = maxAuthCapabilities,
        )
}

/** The engine object a dev constructs exactly once. Holds zero app-lifecycle
 * concepts -- no scene-phase hook and no required environment wrapper.
 * `NMPEngine(NMPConfig(...))` is the entire adoption cost. */
class NMPEngine(
    config: NMPConfig,
    sessionPayload: NMPSessionPayload? = null,
) : AutoCloseable {
    companion object {
        /** Destructively remove one unowned persistent NMP store. A live engine
         * in this OR ANY OTHER process using the same canonical path throws
         * [NMPError.StoreStillOpen] without touching the file; call
         * [shutdown] or [close] first. The refusal is a cross-process exclusive
         * ownership lock (#489), not a process-local guard: constructing a
         * second [NMPEngine] over a live store path throws
         * [NMPError.StoreAlreadyOpen]. */
        fun resetPersistentStore(storePath: String) =
            nmpRethrowing { ffiResetPersistentStore(storePath) }
    }

    internal val ffi: NmpEngine =
        nmpRethrowing { NmpEngine(config.toFfi(), sessionPayload?.ffi) }

    /** The complete set of known accounts and its current reactive selection. */
    val session: NMPSession = NMPSession(ffi)

    /** Install one AUTH policy bound to [publicKey], returning its exact removal proof. */
    fun addAuthPolicy(
        publicKey: String,
        policy: NMPAuthPolicy,
    ): NMPAuthPolicyRegistration =
        NMPAuthPolicyRegistration(
            nmpRethrowing { ffi.addAuthPolicy(publicKey, NMPAuthPolicyBridge(policy)) },
        )

    /** Remove only the policy installation proven by [registration]. The proof remains reusable. */
    fun removeAuthPolicy(registration: NMPAuthPolicyRegistration): Boolean =
        nmpRethrowing { ffi.removeAuthPolicy(registration.ffi) }

    /** Sign one exact event through the current account's provider without accepting or
     * publishing a write. */
    suspend fun signEvent(event: NMPUnsignedEvent): NMPSignedEvent =
        com.nmp.sdk.signEvent(ffi, event)

    // MARK: - Read noun

    /** Open a live, detachable query as a cold `Flow`. Mirrors
     * `docs/builder/30-platform-guides.md`'s PLANNED-shape idiom, now
     * BUILT: the caller applies `stateIn(scope, WhileSubscribed())` (the
     * Room idiom verbatim) if it wants a hot, shared, latest-value read --
     * this SDK never invents its own observer/container type. See
     * Query.kt's `observeQuery` for the teardown-mapping finding. */
    fun observe(filter: NMPFilter): Flow<RowBatch> = observeQuery(ffi, filter)

    /** Open a live, detachable query over an explicit `NMPDemand` (#107) --
     * the constructor to reach for once [observe]'s implicit
     * `AuthorOutboxes`/`Public` default isn't enough: declaring
     * `NMPSourceAuthority.Pinned` wire authority, a non-default
     * `NMPAccessContext`, or a non-`Agnostic` `NMPCacheMode`. One demand is
     * one branch, so this is exactly `observe(NMPLiveQuery.single(demand))`. */
    fun observe(demand: NMPDemand): Flow<RowBatch> =
        observeQuery(ffi, NMPLiveQuery.single(demand))

    /** Open a live, detachable query over several independent `NMPDemand`
     * branches (#1108). The branches are observed through ONE stream: rows
     * are unioned by event id with provenance merged, every batch carries one
     * evidence entry per canonical branch, and one teardown withdraws every
     * branch exactly once. Throws [NMPError.EmptyQueryUnion],
     * [NMPError.AggregateResultLimitZero],
     * [NMPError.NestedAggregateResultLimit] or
     * [NMPError.TooManyQueryBranches] for a declaration that can never be
     * observed. */
    fun observe(query: NMPLiveQuery): Flow<RowBatch> = observeQuery(ffi, query)

    /** Open a bounded, growable observation over the SAME read noun --
     * windowing is a policy parameter on `observe`, not a separate verb.
     * The returned [NMPQuery] owns its conflated full-snapshot
     * [NMPQuery.frames] flow plus the [NMPQuery.requestRows] growth
     * capability; delivery is derived from boundedness (see Window.kt's
     * header for why bounded means snapshots and unbounded means deltas).
     * Throws typed [NMPError.WindowZeroRows] /
     * [NMPError.WindowInitialExceedsMax] /
     * [NMPError.WindowSelectionHasLimit] on an invalid window. */
    fun observe(filter: NMPFilter, window: Window): NMPQuery =
        NMPQuery(nmpRethrowing { ffi.observe(filter.toFfi(), window.toFfi()) })

    /** The explicit-`NMPDemand` windowed overload (#107 x #485): same
     * bounded snapshot/growth discipline as the `NMPFilter` overload, for
     * demands that declare wire authority, access context, or cache mode. */
    fun observe(demand: NMPDemand, window: Window): NMPQuery =
        observe(NMPLiveQuery.single(demand), window)

    /** The explicit-`NMPLiveQuery` windowed overload (#1108 x #485): the
     * window bounds the MERGED union globally, never one window per branch.
     * A live query that already declares an aggregate result limit is refused
     * with [NMPError.WindowAggregateResultLimit] -- a window and an aggregate
     * bound would be two competing owners of the merged row count. */
    fun observe(query: NMPLiveQuery, window: Window): NMPQuery =
        NMPQuery(nmpRethrowing { ffi.observeQuery(query.toFfi(), window.toFfi()) })

    // MARK: - Diagnostics (M5) -- "the acceptance test rendered on screen,
    // permanently": per-relay wire-sub count, the exact wire filters sent,
    // events actually received per relay per kind, and per-filter coverage.
    // Read-only, off the data path -- never influences routing/delivery.

    /** Open a live diagnostics stream as a cold `Flow`, same discipline as
     * `observe`. */
    fun observeDiagnostics(): Flow<DiagnosticsSnapshot> = observeDiagnostics(ffi)

    // nmp-native:if nip02
    // MARK: - NIP-02 (following)

    /** Observe whether the current account follows [target] through the
     * NMP-owned NIP-02 resource. This is NMP's protocol projection, not an
     * app-maintained boolean. See `Following.kt`'s own doc for the
     * conflation/teardown discipline. */
    fun observeFollowing(target: String): Flow<FollowingSnapshot> = observeFollowing(ffi, target)

    /** The simple NMP-owned follow action. It returns immediately with a
     * stream covering acquisition, no-op, atomic conflict, signing,
     * routing, and relay receipt states. */
    fun follow(target: String): FollowAction = follow(ffi, target)

    /** The inverse of [follow], with the same acquisition, compare-and-swap,
     * signer, routing, and receipt guarantees. */
    fun unfollow(target: String): FollowAction = unfollow(ffi, target)
    // nmp-native:endif

    /** Acquire one NIP-11 representation through the shared engine cache. */
    suspend fun relayInformation(
        relay: String,
        policy: RelayInformationCachePolicy = RelayInformationCachePolicy.UseCache,
    ): RelayInformation =
        RelayInformation.from(
            nmpRethrowingAsync { ffi.relayInformation(relay, policy.toFfi()) },
        )

    // MARK: - Write noun

    /** Enqueue a write and return its stable id plus status stream. */
    fun publish(intent: WriteIntent): Receipt = publishReceipt(ffi, intent)

    /** Attach to retained facts without conflating corruption with absence. */
    fun reattachReceipt(id: ULong): ReceiptReattachment = reattachReceipt(ffi, id)

    /** #591: recover a receipt after a crash that happened BEFORE the app
     * could durably persist the receipt id `publish`
     * returned -- looked up by the caller's own crash-safe correlation
     * token instead. Otherwise identical to [reattachReceipt] (the by-id
     * overload). */
    fun reattachReceipt(correlation: String): ReceiptReattachment =
        reattachReceiptByCorrelation(ffi, correlation)

    /** Explicitly cancel an accepted unsigned write by stable receipt id. */
    fun cancel(receiptId: ULong): WriteCancellationOutcome = cancelWrite(ffi, receiptId)

    /** Read the app's own publish queue back.
     *
     * Answers "what have I got outstanding, and what went wrong with it"
     * without having held a receipt stream open since acceptance. This is
     * INSPECTION: it never blocks and never waits for settlement. */
    fun publishQueue(afterReceiptId: ULong? = null, limit: UByte): List<PublishQueueEntry> =
        publishQueue(ffi, afterReceiptId, limit)

    /** Read one bounded page of open obligations for an exact query-row event id. */
    fun publishQueueForEvent(
        eventId: String,
        afterReceiptId: ULong? = null,
        limit: UByte,
    ): List<PublishQueueEntry> = publishQueueForEvent(ffi, eventId, afterReceiptId, limit)

    /** Forget one queue entry.
     *
     * A real TERMINATION path, not housekeeping: a write parked forever on a
     * signer that never attached, and a permanently-failed refused entry, end
     * no other way. A write that still owns open delivery work is refused --
     * cancel that one instead. */
    fun removePublishQueueEntry(receiptId: ULong) = removePublishQueueEntry(ffi, receiptId)

    // MARK: - Lifecycle

    /** Stop the engine. Idempotent. Also called from `close()` (this class
     * is `AutoCloseable`, so `NMPEngine(...).use { ... }` is the JVM
     * `try`-with-resources idiom for scoping the whole engine's lifetime --
     * there is no JVM equivalent of Swift's `deinit` safety net, so unlike
     * `NMPEngine.swift`, forgetting to call `shutdown()`/`close()` here
     * really does leak the engine thread until the JVM exits; this is the
     * sharpest of this falsifier's teardown findings, see README.md). */
    fun shutdown() = ffi.shutdown()

    override fun close() = shutdown()
}
