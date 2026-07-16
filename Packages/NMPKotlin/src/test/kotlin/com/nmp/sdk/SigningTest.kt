package com.nmp.sdk

import java.util.concurrent.atomic.AtomicInteger
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertThrows
import org.junit.jupiter.api.Test
import uniffi.nmp_ffi.FfiSignedEvent

class SigningTest {
    private val secret = "0".repeat(63) + "1"
    private val author = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"

    @Test
    fun signEventReturnsExactBodyWithoutPublishingIt() =
        runBlocking {
            NMPEngine(NMPConfig()).use { engine ->
                assertEquals(author, engine.addAccount(secret).publicKey)
                engine.setActiveAccount(author)
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
                    engine.observe(NMPFilter(kinds = listOf(request.kind))).first().rows,
                    "sign-only must not publish or store the event",
                )
            }
        }

    @Test
    fun signEventWithoutActiveSignerIsTyped() {
        NMPEngine(NMPConfig()).use { engine ->
            engine.setActiveAccount(author)
            assertThrows(NMPError.NoActiveSigner::class.java) {
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

    @Test
    fun signEventCompletionBeforeCancellationHandleInstallIsSafe() =
        runBlocking {
            val request =
                NMPUnsignedEvent(
                    createdAt = 9uL,
                    kind = 1u.toUShort(),
                    tags = emptyList(),
                    content = "immediate",
                )
            val cancellationCalls = AtomicInteger(0)
            val signed =
                signEvent(request) { _, observer ->
                    observer.onSigned(
                        FfiSignedEvent(
                            id = "a".repeat(64),
                            pubkey = author,
                            createdAt = request.createdAt,
                            kind = request.kind,
                            tags = request.tags,
                            content = request.content,
                            sig = "b".repeat(128),
                        ),
                    )
                    val cancel: () -> Unit = { cancellationCalls.incrementAndGet() }
                    cancel
                }

            assertEquals(request.content, signed.content)
            assertEquals(0, cancellationCalls.get())
        }
}
