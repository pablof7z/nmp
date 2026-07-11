// `NMPEngine` -- the ONE construction call the M4 kill test requires (plan
// §7): everything past `init` is a method call on this object, never a
// second container/provider the app must adopt. Kotlin/JVM mirror of
// Engine.swift.

package com.nmp.sdk

import kotlinx.coroutines.flow.Flow
import uniffi.nmp_ffi.NmpEngine
import uniffi.nmp_ffi.NmpEngineConfig

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
) {
    fun toFfi(): NmpEngineConfig =
        NmpEngineConfig(
            storePath = storePath,
            indexerRelays = indexerRelays,
            appRelays = appRelays,
            fallbackRelays = fallbackRelays,
        )
}

/** The engine object a dev constructs exactly once. Holds zero app-lifecycle
 * concepts -- no scene-phase hook, no required provider/environment
 * wrapper. `NMPEngine(NMPConfig(...))` is the entire adoption cost. */
class NMPEngine(config: NMPConfig) : AutoCloseable {
    private val ffi: NmpEngine = nmpRethrowing { NmpEngine(config.toFfi()) }

    // MARK: - Identity (P3; multi-account)

    /** Register an account from its secret key (hex or bech32 `nsec`). The
     * key crosses this boundary exactly once and lives engine-side from
     * this point on. Returns the account's hex public key. Does NOT make
     * the account active -- call `setActiveAccount` for that. */
    fun addAccount(secretKey: String): String = nmpRethrowing { ffi.addAccount(secretKey) }

    /** Re-root every reactive query AND the active signing capability
     * together onto `pubkey` (`null` -> logged-out / read-only browsing). */
    fun setActiveAccount(pubkey: String?) = nmpRethrowing { ffi.setActiveAccount(pubkey) }

    // MARK: - Read noun

    /** Open a live, detachable query as a cold `Flow`. Mirrors
     * `docs/builder/30-platform-guides.md`'s PLANNED-shape idiom, now
     * BUILT: the caller applies `stateIn(scope, WhileSubscribed())` (the
     * Room idiom verbatim) if it wants a hot, shared, latest-value read --
     * this SDK never invents its own observer/container type. See
     * Query.kt's `observeQuery` for the teardown-mapping finding. */
    fun observe(filter: NMPFilter): Flow<RowBatch> = observeQuery(ffi, filter)

    // MARK: - Diagnostics (M5) -- "the acceptance test rendered on screen,
    // permanently": per-relay wire-sub count, the exact wire filters sent,
    // events actually received per relay per kind, and per-filter coverage.
    // Read-only, off the data path -- never influences routing/delivery.

    /** Open a live diagnostics stream as a cold `Flow`, same discipline as
     * `observe`. */
    fun observeDiagnostics(): Flow<DiagnosticsSnapshot> = observeDiagnostics(ffi)

    // MARK: - Write noun

    /** Enqueue a write. Returns a `Flow<WriteStatus>` streaming everything
     * that happens to it after acceptance -- same states as Swift's
     * `Receipt.status`. */
    fun publish(intent: WriteIntent): Flow<WriteStatus> = publishReceipt(ffi, intent)

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
