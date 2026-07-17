// `NMPEngine` -- the ONE construction call the M4 kill test requires (plan
// §7): everything past `init` is a method call on this object, never a
// second container/provider the app must adopt. Kotlin/JVM mirror of
// Engine.swift.

package com.nmp.sdk

import kotlinx.coroutines.flow.Flow
import uniffi.nmp_ffi.NmpEngine
import uniffi.nmp_ffi.NmpEngineConfig
import uniffi.nmp_ffi.generateAccountSecretKey as ffiGenerateAccountSecretKey
import uniffi.nmp_ffi.resetPersistentStore as ffiResetPersistentStore

/** Construction config for `NMPEngine`. The only relay facts this app ever
 * supplies are the three operator-configured lanes -- `indexerRelays`,
 * `appRelays`, `fallbackRelays` -- the engine self-navigates outbox routing
 * from there on its own. No pre-resolved write-relay map is needed or
 * accepted -- mirrors `NMPConfig.swift`'s doc verbatim. */
data class NMPConfig(
    /** `null` -> in-memory store (nothing survives a restart). A path ->
     * a persistent store reopened at that path across launches. */
    val storePath: String? = null,
    val indexerRelays: List<String> = emptyList(),
    /** Operator app relay set (`Lane::AppRelay`) -- every kind, every
     * author, always, additive. Default empty. */
    val appRelays: List<String> = emptyList(),
    /** Operator fallback relay set (`Lane::Fallback`) -- tops up authors
     * under the 2-relay-min, suppressed when `appRelays` is non-empty.
     * Default empty. */
    val fallbackRelays: List<String> = emptyList(),
    /** Local/private relay HOSTS the operator explicitly opts into despite
     * the SSRF admission policy (issue #121). A DISCOVERED (network-sourced
     * kind:10002) relay on a loopback / RFC-1918 / link-local / `.onion`
     * host is rejected by default; listing its host here (e.g. `"127.0.0.1"`
     * or `"localhost"`) re-admits discovered relays on that exact host.
     * Host-only match (port- and path-insensitive). Default empty. */
    val allowedLocalRelayHosts: List<String> = emptyList(),
    /** The one whole-engine relay ceiling. It bounds the complete compiled
     * demand and the transport worker set with the same effective value.
     * Legacy zero is normalized to the finite default, never uncapped. */
    val maxRelays: UInt = 10u,
    /** Finite zero-queue native observer/action/waiter ceiling. */
    val maxNativeTasks: UInt = 12u,
    /** Finite shared ceiling for live signer and AUTH-policy registrations. Zero admits none. */
    val maxAuthCapabilities: UInt = 64u,
) {
    fun toFfi(): NmpEngineConfig =
        NmpEngineConfig(
            storePath = storePath,
            indexerRelays = indexerRelays,
            appRelays = appRelays,
            fallbackRelays = fallbackRelays,
            allowedLocalRelayHosts = allowedLocalRelayHosts,
            maxRelays = maxRelays,
            maxNativeTasks = maxNativeTasks,
            maxAuthCapabilities = maxAuthCapabilities,
        )
}

/** The engine object a dev constructs exactly once. Holds zero app-lifecycle
 * concepts -- no scene-phase hook, no required provider/environment
 * wrapper. `NMPEngine(NMPConfig(...))` is the entire adoption cost.
 *
 * [localAccountStore] accepts ANY conforming [NMPLocalAccountCheckpoint] --
 * a platform-vault provider, an app-custom store, or
 * [NMPInsecureFileAccountStore] -- and, when configured, restores the
 * account it holds on construction: its [NMPLocalAccountCheckpoint.loadSecretKey]
 * drives the restore and the restored account becomes active before the
 * constructor returns. */
class NMPEngine(
    config: NMPConfig,
    private val localAccountStore: NMPLocalAccountCheckpoint? = null,
) : AutoCloseable {
    companion object {
        /** Destructively remove one closed persistent NMP store. A live engine
         * in this process using the same canonical path throws
         * [NMPError.StoreStillOpen] without touching the file; call
         * [shutdown] or [close] first. This guard is process-local. A separate
         * local-account checkpoint is not touched. */
        fun resetPersistentStore(storePath: String) =
            nmpRethrowing { ffiResetPersistentStore(storePath) }
    }

    internal val ffi: NmpEngine = nmpRethrowing { NmpEngine(config.toFfi()) }

    /** Guards [checkpointedPubkey] -- the identity currently persisted in
     * [localAccountStore]'s checkpoint file, `null` when no checkpoint is
     * known to exist -- and [restoredRegistration], the exact
     * [NMPAccountRegistration] created on the init-restore path, when the
     * checkpoint still holds exactly that installation (#589). Tracked so
     * [removeAccount] can clear the checkpoint for exactly the removed
     * identity (#529): without this, a removed account would silently
     * resurrect from the checkpoint on the next engine construction. Also
     * consulted by [detachPersistedAccount] to recover the exact restored
     * registration to detach. */
    private val checkpointLock = Any()
    private var checkpointedPubkey: String? = null
    private var restoredRegistration: NMPAccountRegistration? = null

    init {
        try {
            localAccountStore?.loadSecretKey()?.let { secretKey ->
                val ffiRegistration = nmpRethrowing { ffi.addAccount(secretKey) }
                val registration = NMPAccountRegistration(ffiRegistration)
                nmpRethrowing { ffi.setActiveAccount(registration.publicKey) }
                synchronized(checkpointLock) {
                    checkpointedPubkey = registration.publicKey
                    restoredRegistration = registration
                }
            }
        } catch (error: Throwable) {
            ffi.shutdown()
            throw error
        }
    }

    // MARK: - Identity (P3; multi-account)

    /** Generate and register a brand-new local account (#588) -- the
     * NMP-owned door for a clean-start client that has no existing secret
     * material to hand in. Composes one keygen-only FFI call with the
     * existing [addAccount], so it inherits that method's save-with-rollback
     * choreography and checkpoint tracking wholesale rather than a second,
     * parallel registration pipeline. Mirrors [addAccount]'s own "does not
     * activate" semantics -- call [setActiveAccount] for that. */
    fun generateAccount(): NMPAccountRegistration = addAccount(ffiGenerateAccountSecretKey())

    /** Register an account from its secret key (hex or bech32 `nsec`). The
     * key crosses this boundary exactly once and lives engine-side from
     * this point on. When an [NMPLocalAccountCheckpoint] was explicitly
     * configured, NMP also checkpoints it for restart restoration. Returns
     * an opaque exact-registration proof whose [NMPAccountRegistration.publicKey]
     * identifies the account. Does NOT make the account active -- call
     * [setActiveAccount] for that. */
    fun addAccount(secretKey: String): NMPAccountRegistration {
        val ffiRegistration = nmpRethrowing { ffi.addAccount(secretKey) }
        try {
            localAccountStore?.saveSecretKey(secretKey)
        } catch (persistenceError: Throwable) {
            try {
                if (!nmpRethrowing { ffi.removeAccount(ffiRegistration) }) {
                    persistenceError.addSuppressed(
                        IllegalStateException("exact account rollback was already stale"),
                    )
                }
            } catch (rollbackError: Throwable) {
                persistenceError.addSuppressed(rollbackError)
            }
            throw persistenceError
        }
        if (localAccountStore != null) {
            synchronized(checkpointLock) {
                checkpointedPubkey = ffiRegistration.publicKey()
                // This account -- not whatever the init-restore path
                // installed -- now owns the checkpoint; a later
                // `detachPersistedAccount()` must not fire against a
                // registration the checkpoint no longer (solely) reflects.
                restoredRegistration = null
            }
        }
        return NMPAccountRegistration(ffiRegistration)
    }

    /** Remove only the installation proven by [registration]. The proof
     * remains reusable. When the removal succeeds and [registration] proves
     * the identity currently held by the configured
     * [NMPLocalAccountCheckpoint] checkpoint, NMP also deletes that
     * checkpoint (#529) -- a removed account never resurrects on the next
     * engine construction. A stale proof (`false` return) or a proof for a
     * different identity leaves the checkpoint untouched. */
    fun removeAccount(registration: NMPAccountRegistration): Boolean {
        val removed = nmpRethrowing { ffi.removeAccount(registration.ffi) }
        if (removed) {
            synchronized(checkpointLock) {
                if (checkpointedPubkey == registration.publicKey) {
                    localAccountStore?.clear()
                    checkpointedPubkey = null
                    restoredRegistration = null
                }
            }
        }
        return removed
    }

    /** Detach exactly the local account this engine restored from its
     * configured checkpoint at construction (#589). Delegates to
     * [removeAccount], inheriting its checkpoint-clear behavior verbatim.
     *
     * Returns `false` -- and removes nothing -- when there is no exact
     * restored registration left to detach: no account was restored at
     * construction, a previous [detachPersistedAccount] or [removeAccount]
     * call already spent it, a later [addAccount] has since overwritten the
     * checkpoint with a different installation, or [clearPersistedAccount]
     * was called directly (it also spends the tracked restored registration
     * -- after that call the live restored signer can only be removed by
     * shutting down the engine, never by a later [detachPersistedAccount]
     * call). */
    fun detachPersistedAccount(): Boolean {
        val registration = synchronized(checkpointLock) {
            restoredRegistration?.takeIf { checkpointedPubkey == it.publicKey }
        } ?: return false
        return removeAccount(registration)
    }

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

    /** Re-root every reactive query AND the active signing capability
     * together onto `pubkey` (`null` -> logged-out / read-only browsing). */
    fun setActiveAccount(pubkey: String?) = nmpRethrowing { ffi.setActiveAccount(pubkey) }

    /** The Rust-owned account currently rooting reactive identity and writes. */
    fun activeAccount(): String? = nmpRethrowing { ffi.activeAccount() }

    /** Sign one exact event through the active signer without accepting or
     * publishing a write. */
    suspend fun signEvent(event: NMPUnsignedEvent): NMPSignedEvent =
        com.nmp.sdk.signEvent(ffi, event)

    internal fun nativeTaskCensus() = ffi.nativeTaskCensus()

    internal fun awaitNativeTasksIdle() = ffi.awaitNativeTasksIdle()

    /** Remove the plaintext checkpoint. The live signer remains until close. */
    fun clearPersistedAccount() {
        synchronized(checkpointLock) {
            localAccountStore?.clear()
            checkpointedPubkey = null
            restoredRegistration = null
        }
    }

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
     * `NMPAccessContext`, or a non-`Agnostic` `NMPCacheMode`. Same
     * cold-`Flow`/teardown discipline as the `NMPFilter` overload. */
    fun observe(demand: NMPDemand): Flow<RowBatch> = observeQuery(ffi, demand)

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
        NMPQuery { observer ->
            nmpRethrowing { ffi.observe(filter.toFfi(), window.toFfi(), observer) }
        }

    /** The explicit-`NMPDemand` windowed overload (#107 x #485): same
     * bounded snapshot/growth discipline as the `NMPFilter` overload, for
     * demands that declare wire authority, access context, or cache mode. */
    fun observe(demand: NMPDemand, window: Window): NMPQuery =
        NMPQuery { observer ->
            nmpRethrowing { ffi.observeDemand(demand.toFfi(), window.toFfi(), observer) }
        }

    // MARK: - Diagnostics (M5) -- "the acceptance test rendered on screen,
    // permanently": per-relay wire-sub count, the exact wire filters sent,
    // events actually received per relay per kind, and per-filter coverage.
    // Read-only, off the data path -- never influences routing/delivery.

    /** Open a live diagnostics stream as a cold `Flow`, same discipline as
     * `observe`. */
    fun observeDiagnostics(): Flow<DiagnosticsSnapshot> = observeDiagnostics(ffi)

    // MARK: - NIP-02 (following)

    /** Observe whether the active account follows [target] through the
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

    /** Compose an ordinary kind:9 NIP-29 group message from semantic native
     * state. NMP derives the active author/time, mention content, protocol
     * tags, engine-owned host provenance, and pinned-host routing. */
    fun groupMessageIntent(
        host: String,
        groupId: String,
        content: String,
        recipients: List<String> = emptyList(),
        reply: GroupReplyParent? = null,
    ): GroupSendIntent =
        composeGroupMessageIntent(ffi, host, groupId, content, recipients, reply)

    /** Publish a [GroupSendIntent] from `groupMessageIntent` (#156). Take-once
     * -- see [publishComposedReceipt]'s own doc. */
    fun publishComposed(intent: GroupSendIntent): Receipt = publishComposedReceipt(ffi, intent)

    /** Attach to retained facts without conflating corruption with absence. */
    fun reattachReceipt(id: ULong): ReceiptReattachment = reattachReceipt(ffi, id)

    /** Explicitly cancel an accepted unsigned write by stable receipt id. */
    fun cancel(receiptId: ULong): WriteCancellationOutcome = cancelWrite(ffi, receiptId)

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
