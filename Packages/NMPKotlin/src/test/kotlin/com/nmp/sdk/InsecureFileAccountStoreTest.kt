package com.nmp.sdk

import java.nio.file.Files
import java.nio.file.Path
import java.nio.file.attribute.PosixFilePermission
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertFalse
import org.junit.jupiter.api.Assertions.assertNull
import org.junit.jupiter.api.Assertions.assertThrows
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test
import org.junit.jupiter.api.io.TempDir

class InsecureFileAccountStoreTest {
    @TempDir
    lateinit var root: Path

    private val secretOne = "0".repeat(63) + "1"
    private val publicOne = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"

    /** A NON-SDK conformer: the minimal store an app (or a Keystore-backed
     * provider) would write itself, proving [NMPLocalAccountCheckpoint] is a
     * genuinely public seam, not a blessed-concrete-type parameter. */
    private class InMemoryCheckpoint(private var secretKey: String? = null) :
        NMPLocalAccountCheckpoint {
        override fun loadSecretKey(): String? = secretKey

        override fun saveSecretKey(secretKey: String) {
            this.secretKey = secretKey
        }

        override fun clear() {
            secretKey = null
        }
    }

    /** Any conforming checkpoint store is a drop-in through the public
     * [NMPEngine] constructor: a custom conformer's `loadSecretKey` drives
     * the restore exactly like the SDK's own file store, and [NMPEngine.addAccount]
     * checkpoints back into it. */
    @Test
    fun customCheckpointConformerRestoresThroughPublicConstructor() {
        NMPEngine(NMPConfig(), InMemoryCheckpoint(secretOne)).use { restored ->
            assertEquals(publicOne, restored.activeAccount())
        }

        val empty = InMemoryCheckpoint()
        NMPEngine(NMPConfig(), empty).use { engine ->
            assertNull(engine.activeAccount())
            engine.addAccount(secretOne)
            assertEquals(secretOne, empty.loadSecretKey())
        }
    }

    @Test
    fun checkpointRestoresActiveAccountAndClearReturnsToReadOnly() {
        val checkpoint = root.resolve("local-account.nsec")
        val store = NMPInsecureFileAccountStore(checkpoint)

        NMPEngine(NMPConfig(), store).use { first ->
            val pubkey = first.addAccount(secretOne).publicKey
            first.setActiveAccount(pubkey)
            assertEquals(publicOne, first.activeAccount())
        }

        try {
            assertEquals(
                setOf(PosixFilePermission.OWNER_READ, PosixFilePermission.OWNER_WRITE),
                Files.getPosixFilePermissions(checkpoint),
            )
        } catch (_: UnsupportedOperationException) {
            // The selected test filesystem does not expose POSIX modes.
        }

        NMPEngine(NMPConfig(), store).use { restored ->
            assertEquals(publicOne, restored.activeAccount())
            restored.clearPersistedAccount()
        }
        assertFalse(Files.exists(checkpoint))

        NMPEngine(NMPConfig(), store).use { signedOut ->
            assertNull(signedOut.activeAccount())
        }
    }

    @Test
    fun removedAccountClearsCheckpointAndCannotResurrectOnRestart() {
        val checkpoint = root.resolve("local-account.nsec")
        val store = NMPInsecureFileAccountStore(checkpoint)

        NMPEngine(NMPConfig(), store).use { engine ->
            val registration = engine.addAccount(secretOne)
            assertTrue(Files.exists(checkpoint))
            assertTrue(engine.removeAccount(registration))
            assertFalse(
                Files.exists(checkpoint),
                "removing the checkpointed identity must also remove its checkpoint (#529)",
            )
        }

        NMPEngine(NMPConfig(), store).use { restarted ->
            assertNull(restarted.activeAccount(), "a removed account must not resurrect on restart")
        }
    }

    @Test
    fun staleRegistrationRemovalLeavesCheckpointIntact() {
        val checkpoint = root.resolve("local-account.nsec")
        val store = NMPInsecureFileAccountStore(checkpoint)

        NMPEngine(NMPConfig(), store).use { engine ->
            val stale = engine.addAccount(secretOne)
            val replacement = engine.addAccount(secretOne)
            assertFalse(engine.removeAccount(stale), "a stale proof cannot remove its replacement")
            assertTrue(
                Files.exists(checkpoint),
                "a stale-proof removal must not touch the live checkpoint",
            )
            assertTrue(engine.removeAccount(replacement))
            assertFalse(Files.exists(checkpoint))
        }
    }

    @Test
    fun invalidCheckpointFailsClosedDuringConstruction() {
        val checkpoint = root.resolve("local-account.nsec")
        Files.writeString(checkpoint, "not-a-key")
        val store = NMPInsecureFileAccountStore(checkpoint)

        assertThrows(NMPError.InvalidSecretKey::class.java) {
            NMPEngine(NMPConfig(), store)
        }
    }

    @Test
    fun persistentStoreResetPreservesAccountCheckpoint() {
        val checkpoint = root.resolve("local-account.nsec")
        val database = root.resolve("nmp.redb")
        val store = NMPInsecureFileAccountStore(checkpoint)
        val config = NMPConfig(storePath = database.toString())

        NMPEngine(config, store).use { first ->
            val pubkey = first.addAccount(secretOne).publicKey
            first.setActiveAccount(pubkey)
            assertThrows(NMPError.StoreStillOpen::class.java) {
                NMPEngine.resetPersistentStore(database.toString())
            }
            assertTrue(Files.exists(database), "typed live-store refusal must leave the file intact")
        }

        NMPEngine.resetPersistentStore(database.toString())
        assertFalse(Files.exists(database))
        assertTrue(Files.exists(checkpoint))
        NMPEngine.resetPersistentStore(database.toString())

        NMPEngine(config, store).use { restored ->
            assertEquals(publicOne, restored.activeAccount())
        }
    }
}
