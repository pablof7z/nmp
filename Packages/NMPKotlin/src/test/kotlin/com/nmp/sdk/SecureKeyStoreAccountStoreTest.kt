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
 * Mirrors [InsecureFileAccountStoreTest]'s save -> restore -> clear
 * checkpoint contract for [NMPSecureKeyStoreAccountStore].
 *
 * Unlike the insecure store's test, this one exercises the store's
 * `loadSecretKey` / `saveSecretKey` / `clear` surface directly rather than
 * through `NMPEngine`: `NMPEngine`'s constructor currently types its
 * checkpoint parameter as the concrete `NMPInsecureFileAccountStore?`, and
 * widening that to a shared seam is an `Engine.kt` change intentionally left
 * for a different, file-disjoint slice of issue #47 (see the class doc on
 * [NMPSecureKeyStoreAccountStore]). Exercising the three internal methods
 * directly still proves the exact contract `NMPEngine` depends on: a
 * secret saved before a simulated restart (a brand-new store instance
 * pointed at the same file+password) is restored unchanged, and `clear()`
 * returns the checkpoint to "nothing persisted".
 */
class SecureKeyStoreAccountStoreTest {
    @TempDir
    lateinit var root: Path

    private val secretOne = "0".repeat(63) + "1"
    private val password = "correct horse battery staple".toCharArray()

    @Test
    fun checkpointRestoresAcrossRestartAndClearReturnsToReadOnly() {
        val checkpoint = root.resolve("local-account.keystore")

        // Nothing persisted yet.
        assertNull(NMPSecureKeyStoreAccountStore(checkpoint, password).loadSecretKey())

        NMPSecureKeyStoreAccountStore(checkpoint, password).saveSecretKey(secretOne)
        assertTrue(Files.exists(checkpoint))

        try {
            assertEquals(
                setOf(PosixFilePermission.OWNER_READ, PosixFilePermission.OWNER_WRITE),
                Files.getPosixFilePermissions(checkpoint),
            )
        } catch (_: UnsupportedOperationException) {
            // The selected test filesystem does not expose POSIX modes.
        }

        // Simulated restart: a brand-new store instance, same file + password.
        val restored = NMPSecureKeyStoreAccountStore(checkpoint, password)
        assertEquals(secretOne, restored.loadSecretKey())

        restored.clear()
        assertFalse(Files.exists(checkpoint))

        val signedOut = NMPSecureKeyStoreAccountStore(checkpoint, password)
        assertNull(signedOut.loadSecretKey())
    }

    @Test
    fun overwritingSecretKeyReplacesThePreviousCheckpoint() {
        val checkpoint = root.resolve("local-account.keystore")
        val secretTwo = "0".repeat(63) + "2"

        NMPSecureKeyStoreAccountStore(checkpoint, password).saveSecretKey(secretOne)
        NMPSecureKeyStoreAccountStore(checkpoint, password).saveSecretKey(secretTwo)

        assertEquals(secretTwo, NMPSecureKeyStoreAccountStore(checkpoint, password).loadSecretKey())
    }

    @Test
    fun wrongPasswordFailsClosedRatherThanReturningNullOrGarbage() {
        val checkpoint = root.resolve("local-account.keystore")
        NMPSecureKeyStoreAccountStore(checkpoint, password).saveSecretKey(secretOne)

        val wrongPassword = "not the right passphrase".toCharArray()
        assertThrows(IllegalStateException::class.java) {
            NMPSecureKeyStoreAccountStore(checkpoint, wrongPassword).loadSecretKey()
        }
    }

    @Test
    fun corruptCheckpointFileFailsClosedDuringLoad() {
        val checkpoint = root.resolve("local-account.keystore")
        Files.writeString(checkpoint, "not-a-keystore-file")

        assertThrows(IllegalStateException::class.java) {
            NMPSecureKeyStoreAccountStore(checkpoint, password).loadSecretKey()
        }
    }
}
