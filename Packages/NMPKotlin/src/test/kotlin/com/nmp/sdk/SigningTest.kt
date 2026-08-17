package com.nmp.sdk

import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertThrows
import org.junit.jupiter.api.Test

class SigningTest {
    private val secret = "0".repeat(63) + "1"
    private val author = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"

    @Test
    fun signEventReturnsExactBodyWithoutPublishingIt() =
        runBlocking {
            NMPEngine(NMPConfig()).use { engine ->
                val account = engine.session.add(secret.testPrivateKey(), makeCurrent = true)
                assertEquals(author.testPublicKey(), account.publicKey)
                val request =
                    NMPUnsignedEvent(
                        createdAt = 1_723_456_789uL,
                        kind = 27_272u.toUShort(),
                        tags = listOf(listOf("t", "kotlin-sign-only")),
                        content = "exact kotlin body",
                    )

                val signed = engine.signEvent(request)
                assertEquals(author, signed.pubkey)
                assertEquals(request.createdAt, signed.createdAt)
                assertEquals(request.kind, signed.kind)
                assertEquals(request.tags, signed.tags)
                assertEquals(request.content, signed.content)
                assertEquals(64, signed.id.length)
                assertEquals(128, signed.signature.length)
                assertEquals(
                    emptyList<Row>(),
                    engine.observe(
                            NMPLiveQuery.single(
                                NMPDemand(
                                    selection = NMPFilter(kinds = listOf(request.kind)),
                                )
                            )
                        ).first().rows,
                    "sign-only must not publish or store the event",
                )
            }
        }

    @Test
    fun signEventWithoutCurrentSigningProviderIsTyped() {
        NMPEngine(NMPConfig()).use { engine ->
            engine.session.add(author.testPublicKey(), makeCurrent = true)
            assertThrows(NMPError.NoCurrentSigningProvider::class.java) {
                runBlocking {
                    engine.signEvent(
                        NMPUnsignedEvent(
                            1uL,
                            1u.toUShort(),
                            emptyList(),
                            "body",
                        ),
                    )
                }
            }
        }
    }
}
