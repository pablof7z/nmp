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

    @Test
    fun checkpointRestoresActiveAccountAndClearReturnsToReadOnly() {
        val checkpoint = root.resolve("local-account.nsec")
        val store = NMPInsecureFileAccountStore(checkpoint)

        NMPEngine(NMPConfig(), store).use { first ->
            val pubkey = first.addAccount(secretOne)
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

    // #507/#495: removing the checkpointed account must clear the on-disk
    // checkpoint too, or the removed account resurrects on the very next
    // restore -- proving `addAccount`'s side effect has a symmetric undo.
    // Removing an unrelated pubkey must leave an existing checkpoint alone.
    @Test
    fun removeAccountClearsCheckpointAndLeavesUnrelatedCheckpointIntact() {
        val checkpoint = root.resolve("local-account.nsec")
        val store = NMPInsecureFileAccountStore(checkpoint)
        // The x-only pubkey for secret key `2` -- a distinct, valid, but
        // never-`addAccount`-ed account, used only to prove removal of an
        // unrelated pubkey is a no-op that does not touch the checkpoint.
        val otherPubkey = "c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5"

        NMPEngine(NMPConfig(), store).use { engine ->
            val pubkey = engine.addAccount(secretOne)
            engine.setActiveAccount(pubkey)
            assertTrue(Files.exists(checkpoint))

            assertFalse(
                engine.removeAccount(otherPubkey),
                "an unrelated, never-added pubkey has nothing to remove",
            )
            assertTrue(
                Files.exists(checkpoint),
                "removing an unrelated pubkey must not touch an existing checkpoint",
            )

            assertTrue(engine.removeAccount(pubkey))
            assertFalse(
                Files.exists(checkpoint),
                "removing the checkpointed account must clear its on-disk checkpoint",
            )
        }

        NMPEngine(NMPConfig(), store).use { restarted ->
            assertNull(
                restarted.activeAccount(),
                "a removed-and-cleared account must not resurrect on the next restart",
            )
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
            val pubkey = first.addAccount(secretOne)
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
