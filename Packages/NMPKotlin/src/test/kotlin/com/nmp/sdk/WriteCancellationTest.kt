package com.nmp.sdk

import kotlinx.coroutines.async
import kotlinx.coroutines.flow.toList
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertFalse
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test

class WriteCancellationTest {
    private val author = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"

    @Test
    fun acceptedUnsignedWriteCancelsStreamsAndReattachesAsCancelled() =
        runBlocking {
            NMPEngine(NMPConfig()).use { engine ->
                // A read-only active identity deliberately has no signer. The
                // unsigned write therefore remains accepted and cancellable
                // until the explicit receipt-id transition below.
                engine.setActiveAccount(author)
                val receipt =
                    engine.publish(
                        WriteIntent(
                            payload =
                                WritePayload.Unsigned(
                                    pubkey = author,
                                    createdAt = 1_723_456_790uL,
                                    kind = 1u.toUShort(),
                                    tags = emptyList(),
                                    content = "cancel through the public Kotlin SDK",
                                ),
                            durability = Durability.Durable,
                            routing = WriteRouting.AuthorOutbox,
                        ),
                    )

                val observed = async {
                    withTimeout(5_000) { receipt.status.toList() }
                }
                assertEquals(WriteCancellationOutcome.Cancelled, engine.cancel(receipt.id))

                val statuses = observed.await()
                assertTrue(statuses.contains(WriteStatus.Accepted))
                assertFalse(statuses.any { it is WriteStatus.Signed })
                assertEquals(WriteStatus.Cancelled, statuses.last())

                // The cancellation transition is idempotent and the durable
                // terminal fact is independently reconstructible by id.
                assertEquals(WriteCancellationOutcome.Cancelled, engine.cancel(receipt.id))
                val reattachment = engine.reattachReceipt(receipt.id)
                assertTrue(reattachment is ReceiptReattachment.Attached)
                val replay = (reattachment as ReceiptReattachment.Attached).receipt
                assertEquals(
                    listOf(WriteStatus.Cancelled),
                    withTimeout(5_000) { replay.status.toList() },
                )
            }
        }
}
