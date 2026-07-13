package com.nmp.sdk

import java.nio.file.Files
import java.nio.file.Path
import java.nio.file.attribute.PosixFilePermission
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertFalse
import org.junit.jupiter.api.Assertions.assertNull
import org.junit.jupiter.api.Assertions.assertThrows
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

    @Test
    fun invalidCheckpointFailsClosedDuringConstruction() {
        val checkpoint = root.resolve("local-account.nsec")
        Files.writeString(checkpoint, "not-a-key")
        val store = NMPInsecureFileAccountStore(checkpoint)

        assertThrows(NMPError.InvalidSecretKey::class.java) {
            NMPEngine(NMPConfig(), store)
        }
    }
}
