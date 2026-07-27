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

/**
 * Mirrors [SecureKeyStoreAccountStoreTest]'s save -> restore -> clear
 * checkpoint contract for [NMPSecureKeyStoreNip46SessionCheckpointStore]
 * (#571): a secret saved before a simulated restart (a brand-new store
 * instance pointed at the same file+password) is restored unchanged, and
 * `clear()` returns the checkpoint to "nothing persisted".
 */
class SecureKeyStoreNip46SessionCheckpointStoreTest {
    @TempDir
    lateinit var root: Path

    private val password = "correct horse battery staple".toCharArray()
    private val checkpointOne = NMPNip46SessionCheckpoint(
        clientSecretKey = "1".repeat(64),
        userPublicKey = "2".repeat(64),
        remoteSignerPublicKey = "3".repeat(64),
        relays = listOf("wss://relay.example"),
        origin = NMPNip46SessionOrigin.ClientInitiated,
    )

    @Test
    fun checkpointRestoresAcrossRestartAndClearReturnsToEmpty() {
        val checkpointFile = root.resolve("nip46-session.keystore")

        assertNull(NMPSecureKeyStoreNip46SessionCheckpointStore(checkpointFile, password).loadCheckpoint())

        NMPSecureKeyStoreNip46SessionCheckpointStore(checkpointFile, password)
            .saveCheckpoint(checkpointOne)
        assertTrue(Files.exists(checkpointFile))

        try {
            assertEquals(
                setOf(PosixFilePermission.OWNER_READ, PosixFilePermission.OWNER_WRITE),
                Files.getPosixFilePermissions(checkpointFile),
            )
        } catch (_: UnsupportedOperationException) {
            // The selected test filesystem does not expose POSIX modes.
        }

        // Simulated restart: a brand-new store instance, same file + password.
        val restored = NMPSecureKeyStoreNip46SessionCheckpointStore(checkpointFile, password)
        assertEquals(checkpointOne, restored.loadCheckpoint())

        restored.clear()
        assertFalse(Files.exists(checkpointFile))

        val signedOut = NMPSecureKeyStoreNip46SessionCheckpointStore(checkpointFile, password)
        assertNull(signedOut.loadCheckpoint())
    }

    @Test
    fun overwritingCheckpointReplacesThePreviousOne() {
        val checkpointFile = root.resolve("nip46-session.keystore")
        val checkpointTwo = NMPNip46SessionCheckpoint(
            clientSecretKey = "4".repeat(64),
            userPublicKey = "5".repeat(64),
            remoteSignerPublicKey = "6".repeat(64),
            relays = listOf("wss://relay2.example"),
            origin = NMPNip46SessionOrigin.Bunker,
        )

        NMPSecureKeyStoreNip46SessionCheckpointStore(checkpointFile, password)
            .saveCheckpoint(checkpointOne)
        NMPSecureKeyStoreNip46SessionCheckpointStore(checkpointFile, password)
            .saveCheckpoint(checkpointTwo)

        assertEquals(
            checkpointTwo,
            NMPSecureKeyStoreNip46SessionCheckpointStore(checkpointFile, password).loadCheckpoint(),
        )
    }

    @Test
    fun wrongPasswordFailsClosedRatherThanReturningNullOrGarbage() {
        val checkpointFile = root.resolve("nip46-session.keystore")
        NMPSecureKeyStoreNip46SessionCheckpointStore(checkpointFile, password)
            .saveCheckpoint(checkpointOne)

        val wrongPassword = "not the right passphrase".toCharArray()
        assertThrows(IllegalStateException::class.java) {
            NMPSecureKeyStoreNip46SessionCheckpointStore(checkpointFile, wrongPassword).loadCheckpoint()
        }
    }

    @Test
    fun corruptCheckpointFileFailsClosedDuringLoad() {
        val checkpointFile = root.resolve("nip46-session.keystore")
        Files.writeString(checkpointFile, "not-a-keystore-file")

        assertThrows(IllegalStateException::class.java) {
            NMPSecureKeyStoreNip46SessionCheckpointStore(checkpointFile, password).loadCheckpoint()
        }
    }
}
