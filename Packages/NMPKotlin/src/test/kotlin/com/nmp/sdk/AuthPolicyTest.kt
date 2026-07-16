package com.nmp.sdk

import java.nio.file.FileAlreadyExistsException
import java.nio.file.Files
import java.nio.file.Path
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertFalse
import org.junit.jupiter.api.Assertions.assertThrows
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test
import org.junit.jupiter.api.io.TempDir
import uniffi.nmp_ffi.FfiAccessContext
import uniffi.nmp_ffi.FfiAuthDiagnostics
import uniffi.nmp_ffi.FfiAuthPhase
import uniffi.nmp_ffi.FfiAuthPolicyCompletionException
import uniffi.nmp_ffi.FfiAuthPolicyOutcome
import uniffi.nmp_ffi.FfiAuthPolicyRequest

class AuthPolicyTest {
    @TempDir
    lateinit var root: Path

    private val secret = "0".repeat(63) + "1"
    private val publicKey = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"

    @Test
    fun accountRegistrationsAreExactBorrowedAndStaleSafe() {
        NMPEngine(NMPConfig(maxAuthCapabilities = 1u)).use { engine ->
            val original = engine.addAccount(secret)
            assertEquals(publicKey, original.publicKey)
            assertThrows(NMPError.AuthCapabilityRegistryFull::class.java) {
                engine.addAccount("0".repeat(63) + "2")
            }
            val replacement = engine.addAccount(secret)
            assertFalse(engine.removeAccount(original), "a stale proof cannot remove its replacement")
            assertTrue(engine.removeAccount(replacement))
            assertFalse(engine.removeAccount(replacement), "repeated borrowed removal is harmless")
        }
    }

    @Test
    fun authPolicyRegistrationsAreExactBorrowedAndShareCapacity() {
        NMPEngine(NMPConfig(maxAuthCapabilities = 1u)).use { engine ->
            val policy = RecordingPolicy()
            val original = engine.addAuthPolicy(publicKey, policy)
            assertEquals(publicKey, original.publicKey)
            assertThrows(NMPError.AuthCapabilityRegistryFull::class.java) {
                engine.addAccount(secret)
            }

            val replacement = engine.addAuthPolicy(publicKey, policy)
            assertFalse(engine.removeAuthPolicy(original), "a stale proof cannot remove its replacement")
            assertTrue(engine.removeAuthPolicy(replacement))
            assertFalse(engine.removeAuthPolicy(replacement), "repeated borrowed removal is harmless")
        }
    }

    @Test
    fun zeroCapabilityCeilingAdmitsNeitherSignerNorPolicy() {
        val config = NMPConfig(maxAuthCapabilities = 0u)
        assertEquals(0u, config.toFfi().maxAuthCapabilities)
        NMPEngine(config).use { engine ->
            assertThrows(NMPError.AuthCapabilityRegistryFull::class.java) {
                engine.addAccount(secret)
            }
            assertThrows(NMPError.AuthCapabilityRegistryFull::class.java) {
                engine.addAuthPolicy(publicKey, RecordingPolicy())
            }
        }
    }

    @Test
    fun failedCheckpointRollsBackExactLiveAccountAndPreservesOriginalError() {
        val blockingParent = root.resolve("not-a-directory")
        Files.createFile(blockingParent)
        val store = NMPInsecureFileAccountStore(blockingParent.resolve("account.nsec"))

        NMPEngine(NMPConfig(maxAuthCapabilities = 1u), store).use { engine ->
            val persistenceError =
                assertThrows(FileAlreadyExistsException::class.java) {
                    engine.addAccount(secret)
                }
            assertTrue(
                persistenceError.suppressed.isEmpty(),
                "the exact rollback should succeed without obscuring the checkpoint failure",
            )
            assertEquals(null, engine.activeAccount())

            Files.delete(blockingParent)
            Files.createDirectory(blockingParent)
            val retry = engine.addAccount(secret)
            assertEquals(publicKey, retry.publicKey)
            assertTrue(Files.exists(blockingParent.resolve("account.nsec")))
            assertTrue(engine.removeAccount(retry), "retry proves the failed add leaked no capability")
        }
    }

    @Test
    fun callbackScopeDistinguishesInlinePendingAndUnavailableDeterministically() {
        val inlinePeer = RecordingCompletionPeer()
        val inline = NMPAuthPolicyCompletion(inlinePeer)
        inline.resolve(NMPAuthPolicyOutcome.Allow)
        inline.releaseCallbackScope()
        assertEquals(listOf(FfiAuthPolicyOutcome.Allow), inlinePeer.outcomes)
        assertEquals(1, inlinePeer.closes)

        val pendingPeer = RecordingCompletionPeer()
        val pending = NMPAuthPolicyCompletion(pendingPeer).retain()
        pending.releaseCallbackScope()
        assertEquals(0, pendingPeer.closes)
        pending.resolve(NMPAuthPolicyOutcome.Deny("no"))
        assertEquals(listOf(FfiAuthPolicyOutcome.Deny("no")), pendingPeer.outcomes)

        val unavailablePeer = RecordingCompletionPeer()
        NMPAuthPolicyCompletion(unavailablePeer).releaseCallbackScope()
        assertEquals(1, unavailablePeer.closes)
        assertTrue(unavailablePeer.outcomes.isEmpty())
    }

    @Test
    fun completionExposesOnlyExactTerminalErrors() {
        assertTerminalError(
            FfiAuthPolicyCompletionException.AlreadyCompleted(),
            NMPAuthPolicyCompletionError.AlreadyCompleted::class.java,
        )
        assertTerminalError(
            FfiAuthPolicyCompletionException.Cancelled(),
            NMPAuthPolicyCompletionError.Cancelled::class.java,
        )
        assertTerminalError(
            FfiAuthPolicyCompletionException.ReceiverGone(),
            NMPAuthPolicyCompletionError.ReceiverGone::class.java,
        )
    }

    @Test
    fun cancellationDeliversEveryExactRequestFieldAndAllowsEngineReentry() {
        NMPEngine(NMPConfig()).use { engine ->
            var delivered: NMPAuthPolicyRequest? = null
            var reentered = false
            val policy =
                object : NMPAuthPolicy {
                    override fun evaluate(
                        request: NMPAuthPolicyRequest,
                        completion: NMPAuthPolicyCompletion,
                    ) {}

                    override fun onCancelled(request: NMPAuthPolicyRequest) {
                        delivered = request
                        assertEquals(null, engine.activeAccount())
                        reentered = true
                    }
                }

            NMPAuthPolicyBridge(policy).onCancelled(
                FfiAuthPolicyRequest(
                    expectedPublicKey = publicKey,
                    relay = "wss://auth.example",
                    challenge = "challenge-exact",
                    transportGeneration = 17uL,
                    epochSequence = 23uL,
                ),
            )

            assertEquals(
                NMPAuthPolicyRequest(
                    publicKey = publicKey,
                    relay = "wss://auth.example",
                    challenge = "challenge-exact",
                    transportGeneration = 17uL,
                    epochSequence = 23uL,
                ),
                delivered,
            )
            assertTrue(reentered)
        }
    }

    @Test
    fun authDiagnosticsProjectEveryExactFieldAndPhase() {
        FfiAuthPhase.entries.forEach { ffiPhase ->
            val projected =
                AuthDiagnostics.from(
                    FfiAuthDiagnostics(
                        relay = "wss://auth.example",
                        access = FfiAccessContext.Nip42(publicKey),
                        transportGeneration = 17uL,
                        epochSequence = 23uL,
                        challengeDescriptor = "blake3:challenge",
                        phase = ffiPhase,
                        policyBound = true,
                        signerBound = false,
                        authEventId = "e".repeat(64),
                        sendHandoffAccepted = true,
                        relayOkAccepted = false,
                    ),
                )

            assertEquals("wss://auth.example", projected.relay)
            assertEquals(NMPAccessContext.Nip42(publicKey), projected.access)
            assertEquals(17uL, projected.transportGeneration)
            assertEquals(23uL, projected.epochSequence)
            assertEquals("blake3:challenge", projected.challengeDescriptor)
            assertEquals(AuthPhase.from(ffiPhase), projected.phase)
            assertTrue(projected.policyBound)
            assertFalse(projected.signerBound)
            assertEquals("e".repeat(64), projected.authEventId)
            assertTrue(projected.sendHandoffAccepted)
            assertFalse(projected.relayOkAccepted)
        }
    }

    private fun assertTerminalError(
        ffiError: Exception,
        expected: Class<out NMPAuthPolicyCompletionError>,
    ) {
        val completion = NMPAuthPolicyCompletion(RecordingCompletionPeer(ffiError)).retain()
        assertThrows(expected) { completion.resolve(NMPAuthPolicyOutcome.Allow) }
    }

    private class RecordingPolicy : NMPAuthPolicy {
        override fun evaluate(
            request: NMPAuthPolicyRequest,
            completion: NMPAuthPolicyCompletion,
        ) {}
    }

    private class RecordingCompletionPeer(
        private val error: Exception? = null,
    ) : AuthPolicyCompletionPeer {
        val outcomes = mutableListOf<FfiAuthPolicyOutcome>()
        var closes = 0

        override fun resolve(outcome: FfiAuthPolicyOutcome) {
            error?.let { throw it }
            outcomes += outcome
        }

        override fun close() {
            closes += 1
        }
    }
}
