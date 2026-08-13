package com.nmp.sdk

import org.junit.jupiter.api.Assertions.assertArrayEquals
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertFalse
import org.junit.jupiter.api.Assertions.assertNull
import org.junit.jupiter.api.Assertions.assertSame
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test
import uniffi.nmp_ffi.FfiSessionPayload

class SessionTest {
    private val secret = "0".repeat(63) + "1"
    private val signerPublicKey =
        "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"
    private val publicKeyOnly =
        "c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5"

    @Test
    fun decodedKeyValuesKeepBech32AndHexOutsideTheSessionBoundary() {
        val publicKey = publicKeyOnly.testPublicKey()
        val firstCopy = publicKey.bytes
        firstCopy.fill(0)

        assertArrayEquals(publicKeyOnly.hexBytes(), publicKey.bytes)
        assertEquals("NMPPrivateKey(<redacted>)", secret.testPrivateKey().toString())
    }

    @Test
    fun nativeGeneratedPrivateKeyCanBackAnAccountWithoutExposingItsBytes() {
        val privateKey = NMPPrivateKey.generate()

        NMPEngine(NMPConfig()).use { engine ->
            val account = engine.session.add(privateKey)

            assertEquals(NMPSessionProviderKind.LocalKey, account.providerKind)
            assertEquals(NMPCapabilityAvailability.Available, account.signingAvailability)
        }
    }

    @Test
    fun exportedPayloadWrapperRetainsTheNativePayloadObject() {
        val ffi = FfiSessionPayload.fromBytes(byteArrayOf(1, 2, 3))
        val payload = NMPSessionPayload(ffi)

        assertSame(ffi, payload.ffi)
        assertArrayEquals(byteArrayOf(1, 2, 3), payload.bytes())
    }

    @Test
    fun wholeSessionRestoresSignerBackedAndPublicKeyOnlyAccounts() {
        val payload =
            NMPEngine(NMPConfig()).use { engine ->
                val signer =
                    engine.session.add(secret.testPrivateKey(), makeCurrent = true)
                val readOnly = engine.session.add(publicKeyOnly.testPublicKey())

                assertEquals(signerPublicKey.testPublicKey(), signer.publicKey)
                assertEquals(NMPSessionProviderKind.LocalKey, signer.providerKind)
                assertEquals(NMPCapabilityAvailability.Available, signer.signingAvailability)
                assertNull(readOnly.providerKind)
                assertEquals(NMPCapabilityAvailability.Unsupported, readOnly.signingAvailability)
                assertEquals(signerPublicKey.testPublicKey(), engine.session.current?.publicKey)

                engine.session.export()
            }

        assertTrue(payload.bytes().isNotEmpty())
        assertEquals("NMPSessionPayload(<redacted>)", payload.toString())

        val copied = NMPSessionPayload(payload.bytes())
        assertArrayEquals(payload.bytes(), copied.bytes())

        NMPEngine(NMPConfig(), sessionPayload = copied).use { restored ->
            assertEquals(
                setOf(signerPublicKey.testPublicKey(), publicKeyOnly.testPublicKey()),
                restored.session.accounts.map { it.publicKey }.toSet(),
            )
            assertEquals(signerPublicKey.testPublicKey(), restored.session.current?.publicKey)

            val restoredPublicKeyOnly =
                restored.session.accounts.single {
                    it.publicKey == publicKeyOnly.testPublicKey()
                }
            assertNull(restoredPublicKeyOnly.providerKind)
            assertEquals(
                NMPCapabilityAvailability.Unsupported,
                restoredPublicKeyOnly.signingAvailability,
            )
        }
    }

    @Test
    fun removingCurrentAccountClearsSelectionAndRemovalHasOneMeaning() {
        NMPEngine(NMPConfig()).use { engine ->
            val account =
                engine.session.add(secret.testPrivateKey(), makeCurrent = true)

            assertTrue(engine.session.remove(account))
            assertTrue(engine.session.accounts.isEmpty())
            assertNull(engine.session.current)
            assertFalse(engine.session.remove(account), "repeated removal must be a no-op")
        }
    }

    @Test
    fun makeCurrentAndClearOperateOnTheWholeSession() {
        NMPEngine(NMPConfig()).use { engine ->
            val signer = engine.session.add(secret.testPrivateKey())
            val readOnly = engine.session.add(publicKeyOnly.testPublicKey())

            engine.session.makeCurrent(readOnly)
            assertEquals(publicKeyOnly.testPublicKey(), engine.session.current?.publicKey)

            engine.session.clear()
            assertTrue(engine.session.accounts.isEmpty())
            assertNull(engine.session.current)
            assertFalse(engine.session.remove(signer))
        }
    }
}
