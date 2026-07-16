package com.nmp.sdk

import java.nio.file.Files
import java.nio.file.Path
import kotlinx.coroutines.runBlocking
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertFalse
import org.junit.jupiter.api.Assertions.assertNotEquals
import org.junit.jupiter.api.Assertions.assertNull
import org.junit.jupiter.api.Assertions.assertThrows
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test
import org.junit.jupiter.api.io.TempDir

/**
 * #588: [NMPEngine.generateAccount] -- the NMP-owned keygen door for a
 * clean-start client, composed from one keygen-only FFI call plus the
 * existing [NMPEngine.addAccount]. Its atomicity/checkpoint/removal behavior
 * is inherited wholesale from `addAccount`'s own suite
 * (`InsecureFileAccountStoreTest.kt`); these tests exercise the composition
 * itself. Kotlin mirror of `GenerateAccountTests.swift`.
 */
class GenerateAccountTest {
    @TempDir
    lateinit var root: Path

    private class CheckpointFailure : RuntimeException("injected")

    /** Mirrors `InsecureFileAccountStoreTest`-style fault injection: fails
     * the first `saveSecretKey` so `generateAccount()` exercises
     * `addAccount`'s inherited rollback choreography. */
    private class FailOnceCheckpoint : NMPLocalAccountCheckpoint {
        private var shouldFail = true
        private var secretKey: String? = null

        override fun loadSecretKey(): String? = secretKey

        override fun saveSecretKey(secretKey: String) {
            if (shouldFail) {
                shouldFail = false
                throw CheckpointFailure()
            }
            this.secretKey = secretKey
        }

        override fun clear() {
            secretKey = null
        }
    }

    @Test
    fun generateAccountCreatesFreshDistinctAccountsAndDoesNotActivate() {
        NMPEngine(NMPConfig(maxAuthCapabilities = 4u)).use { engine ->
            val first = engine.generateAccount()
            val second = engine.generateAccount()

            assertNotEquals(first.publicKey, second.publicKey, "each generated account must be fresh")
            assertEquals(64, first.publicKey.length, "public key must be a 32-byte hex string")
            assertNull(
                engine.activeAccount(),
                "generateAccount must not activate the account, mirroring addAccount",
            )
        }
    }

    @Test
    fun generateAccountRestartRestoreRoundTrip() {
        val checkpoint = root.resolve("local-account.nsec")
        val store = NMPInsecureFileAccountStore(checkpoint)

        val publicKey =
            NMPEngine(NMPConfig(), store).use { first ->
                val registration = first.generateAccount()
                first.setActiveAccount(registration.publicKey)
                assertEquals(registration.publicKey, first.activeAccount())
                registration.publicKey
            }

        NMPEngine(NMPConfig(), store).use { restored ->
            assertEquals(
                publicKey,
                restored.activeAccount(),
                "a generated account checkpoints and restores exactly like addAccount",
            )
        }
    }

    @Test
    fun generateAccountRegistrationCanBeRemoved() =
        runBlocking {
            NMPEngine(NMPConfig(maxAuthCapabilities = 4u)).use { engine ->
                val registration = engine.generateAccount()
                engine.setActiveAccount(registration.publicKey)
                engine.signEvent(NMPUnsignedEvent(1uL, 1u.toUShort(), emptyList(), "generated lifecycle"))

                assertTrue(engine.removeAccount(registration))
                assertThrows(NMPError.NoActiveSigner::class.java) {
                    runBlocking {
                        engine.signEvent(
                            NMPUnsignedEvent(1uL, 1u.toUShort(), emptyList(), "generated lifecycle"),
                        )
                    }
                }
                assertFalse(engine.removeAccount(registration), "repeated removal must be stale-safe")
            }
        }

    @Test
    fun generateAccountCheckpointFailureRollsBackAndRetrySucceeds() =
        runBlocking {
            val checkpoint = FailOnceCheckpoint()
            NMPEngine(NMPConfig(maxAuthCapabilities = 1u), checkpoint).use { engine ->
                assertThrows(CheckpointFailure::class.java) {
                    engine.generateAccount()
                }
                assertNull(checkpoint.loadSecretKey())

                val registration = engine.generateAccount()
                engine.setActiveAccount(registration.publicKey)
                engine.signEvent(NMPUnsignedEvent(1uL, 1u.toUShort(), emptyList(), "generated lifecycle"))
                assertEquals(64, checkpoint.loadSecretKey()?.length)
            }
        }
}
