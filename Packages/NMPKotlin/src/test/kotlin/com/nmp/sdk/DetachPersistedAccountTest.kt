package com.nmp.sdk

import java.nio.file.Files
import java.nio.file.Path
import kotlinx.coroutines.runBlocking
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertFalse
import org.junit.jupiter.api.Assertions.assertNull
import org.junit.jupiter.api.Assertions.assertThrows
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test
import org.junit.jupiter.api.io.TempDir

/**
 * #589: [NMPEngine.detachPersistedAccount] -- the exact-registration detach
 * for whichever account this engine restored from its configured checkpoint
 * at construction. Wrapper-only: it is [NMPEngine.removeAccount]'s
 * already-durable checkpoint-clear contract, reused verbatim, applied to the
 * init-restore registration the engine now also retains. Kotlin mirror of
 * `DetachPersistedAccountTests.swift`.
 */
class DetachPersistedAccountTest {
    @TempDir
    lateinit var root: Path

    private class ClearFailure : RuntimeException("injected")

    /** A checkpoint whose `clear()` fails exactly once, to deterministically
     * exercise the checkpoint-clear failure's recovery path (retry via
     * [NMPEngine.clearPersistedAccount]) without a real filesystem failure. */
    private class FailOnceOnClearCheckpoint(private var secretKey: String?) :
        NMPLocalAccountCheckpoint {
        private var shouldFailClear = true

        override fun loadSecretKey(): String? = secretKey

        override fun saveSecretKey(secretKey: String) {
            this.secretKey = secretKey
        }

        override fun clear() {
            if (shouldFailClear) {
                shouldFailClear = false
                throw ClearFailure()
            }
            secretKey = null
        }
    }

    private val secretOne = "0".repeat(63) + "1"
    private val secretTwo = "0".repeat(63) + "2"
    private val publicOne = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"

    @Test
    fun coldRestoreThenDetachClearsCheckpointSignerAndDoesNotResurrect() =
        runBlocking {
            val checkpoint = root.resolve("local-account.nsec")
            val store = NMPInsecureFileAccountStore(checkpoint)

            NMPEngine(NMPConfig(), store).use { seed ->
                val registration = seed.addAccount(secretOne)
                seed.setActiveAccount(registration.publicKey)
            }
            assertTrue(Files.exists(checkpoint))

            NMPEngine(NMPConfig(), store).use { restored ->
                assertEquals(publicOne, restored.activeAccount())
                assertTrue(restored.detachPersistedAccount())
                assertFalse(
                    Files.exists(checkpoint),
                    "detach must clear the on-disk checkpoint like removeAccount does",
                )
                assertThrows(NMPError.NoActiveSigner::class.java) {
                    runBlocking {
                        restored.signEvent(NMPUnsignedEvent(1uL, 1u.toUShort(), emptyList(), "body"))
                    }
                }
            }

            NMPEngine(NMPConfig(), store).use { next ->
                assertNull(
                    next.activeAccount(),
                    "a detached account must not resurrect on next launch",
                )
            }
        }

    @Test
    fun repeatedDetachReturnsFalse() =
        runBlocking {
            val checkpoint = root.resolve("local-account.nsec")
            val store = NMPInsecureFileAccountStore(checkpoint)

            NMPEngine(NMPConfig(), store).use { seed ->
                val registration = seed.addAccount(secretOne)
                seed.setActiveAccount(registration.publicKey)
            }

            NMPEngine(NMPConfig(), store).use { engine ->
                assertTrue(engine.detachPersistedAccount())
                assertFalse(
                    engine.detachPersistedAccount(),
                    "a second detach on an already-spent registration must be a stale-safe no-op",
                )
            }
        }

    @Test
    fun detachWithNoRestoredAccountReturnsFalse() {
        val checkpoint = root.resolve("local-account.nsec")
        val store = NMPInsecureFileAccountStore(checkpoint)

        // Nothing was ever checkpointed -- construction restores nothing.
        NMPEngine(NMPConfig(), store).use { engine ->
            assertFalse(engine.detachPersistedAccount())
        }

        // No checkpoint store configured at all.
        NMPEngine(NMPConfig()).use { bare ->
            assertFalse(bare.detachPersistedAccount())
        }
    }

    @Test
    fun detachAfterLaterAddAccountOverwriteReturnsFalse() =
        runBlocking {
            val checkpoint = root.resolve("local-account.nsec")
            val store = NMPInsecureFileAccountStore(checkpoint)

            NMPEngine(NMPConfig(), store).use { seed ->
                val registration = seed.addAccount(secretOne)
                seed.setActiveAccount(registration.publicKey)
            }

            NMPEngine(NMPConfig(), store).use { engine ->
                assertEquals(publicOne, engine.activeAccount())

                // A later `addAccount` overwrites the on-disk checkpoint with a
                // different installation; the originally-restored registration
                // is no longer the one the checkpoint holds.
                engine.addAccount(secretTwo)

                assertFalse(
                    engine.detachPersistedAccount(),
                    "detach must not fire once a later addAccount has overwritten the checkpoint",
                )
            }
        }

    @Test
    fun detachAfterCheckpointClearFailureIsRecoverableViaClearPersistedAccount() {
        val checkpoint = FailOnceOnClearCheckpoint(secretOne)
        NMPEngine(NMPConfig(), checkpoint).use { engine ->
            assertEquals(publicOne, engine.activeAccount())

            assertThrows(ClearFailure::class.java) {
                engine.detachPersistedAccount()
            }

            // Documented recovery: the engine-side removal already stood (the
            // registration is spent), so a second detach is stale-safe...
            assertFalse(engine.detachPersistedAccount())
            // ...and the caller retries the file cleanup directly.
            engine.clearPersistedAccount()
            assertNull(checkpoint.loadSecretKey())
        }
    }

    @Test
    fun canonicalStoreAndCachePreservedAcrossDetach() =
        runBlocking {
            val checkpoint = root.resolve("local-account.nsec")
            val database = root.resolve("nmp.redb")
            val store = NMPInsecureFileAccountStore(checkpoint)
            val config = NMPConfig(storePath = database.toString())

            NMPEngine(config, store).use { seed ->
                val registration = seed.addAccount(secretOne)
                seed.setActiveAccount(registration.publicKey)
            }

            NMPEngine(config, store).use { restored ->
                assertTrue(restored.detachPersistedAccount())
            }

            assertTrue(
                Files.exists(database),
                "detach must never touch the canonical NMP store",
            )

            NMPEngine(config, store).use { reopened ->
                assertNull(reopened.activeAccount())
            }
        }
}
