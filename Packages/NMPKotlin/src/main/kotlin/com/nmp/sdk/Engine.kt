// `NMPEngine` -- the ONE construction call the M4 kill test requires (plan
// §7): everything past `init` is a method call on this object, never a
// second container/provider the app must adopt. Kotlin/JVM mirror of
// Engine.swift.

package com.nmp.sdk

import kotlinx.coroutines.flow.Flow
import uniffi.nmp_ffi.NmpEngine
import uniffi.nmp_ffi.NmpEngineConfig
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
        )
}

/** The engine object a dev constructs exactly once. Holds zero app-lifecycle
 * concepts -- no scene-phase hook, no required provider/environment
 * wrapper. `NMPEngine(NMPConfig(...))` is the entire adoption cost. */
class NMPEngine(
    config: NMPConfig,
    private val localAccountStore: NMPInsecureFileAccountStore? = null,
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

    init {
        try {
            localAccountStore?.loadSecretKey()?.let { secretKey ->
                val pubkey = nmpRethrowing { ffi.addAccount(secretKey) }
                nmpRethrowing { ffi.setActiveAccount(pubkey) }
            }
        } catch (error: Throwable) {
            ffi.shutdown()
            throw error
        }
    }

    // MARK: - Identity (P3; multi-account)

    /** Register an account from its secret key (hex or bech32 `nsec`). The
     * key crosses this boundary exactly once and lives engine-side from
     * this point on. When an [NMPInsecureFileAccountStore] was explicitly
     * configured, NMP also checkpoints it for restart restoration. Returns
     * the account's hex public key. Does NOT make the account active -- call
     * [setActiveAccount] for that. */
    fun addAccount(secretKey: String): String {
        val pubkey = nmpRethrowing { ffi.addAccount(secretKey) }
        localAccountStore?.saveSecretKey(secretKey)
        return pubkey
    }

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
    fun clearPersistedAccount() = localAccountStore?.clear()

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
