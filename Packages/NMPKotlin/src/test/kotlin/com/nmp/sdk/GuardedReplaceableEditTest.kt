package com.nmp.sdk

import kotlinx.coroutines.flow.take
import kotlinx.coroutines.flow.toList
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertNotEquals
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test

/** #597: a native caller can guard a complete, arbitrary replaceable event
 * without a kind-specific helper. These are real engine/store acceptance
 * paths: exact/null guards sign, while the stale guard never reaches signing. */
class GuardedReplaceableEditTest {
    private val secret = "0".repeat(63) + "1"

    private suspend fun signedId(receipt: Receipt): String {
        val statuses = withTimeout(5_000) { receipt.status.take(2).toList() }
        assertEquals(WriteStatus.Accepted, statuses[0])
        assertTrue(statuses[1] is WriteStatus.Signed)
        return (statuses[1] as WriteStatus.Signed).eventId
    }

    @Test
    fun genericReplaceableGuardExactConflictAndFirstCreation() =
        runBlocking {
            NMPEngine(NMPConfig()).use { engine ->
                val account = engine.addAccount(secret)
                engine.setActiveAccount(account.publicKey)

                val base =
                    engine.publish(
                        WriteIntent(
                            payload =
                                WritePayload.Unsigned(
                                    pubkey = account.publicKey,
                                    createdAt = 1_723_460_000uL,
                                    kind = 10_042u,
                                    tags = listOf(listOf("x", "caller-owned")),
                                    content = "base",
                                ),
                            durability = Durability.Durable,
                            routing = WriteRouting.AuthorOutbox,
                        ),
                    )
                val baseId = signedId(base)

                val exact =
                    engine.publish(
                        WriteIntent(
                            payload =
                                WritePayload.UnsignedReplaceableEdit(
                                    pubkey = account.publicKey,
                                    createdAt = 1_723_460_001uL,
                                    kind = 10_042u,
                                    tags = listOf(listOf("x", "caller-owned")),
                                    content = "exact replacement",
                                    expectedBase = baseId,
                                ),
                            durability = Durability.Durable,
                            routing = WriteRouting.AuthorOutbox,
                        ),
                    )
                val exactId = signedId(exact)
                assertNotEquals(baseId, exactId)

                val stale =
                    engine.publish(
                        WriteIntent(
                            payload =
                                WritePayload.UnsignedReplaceableEdit(
                                    pubkey = account.publicKey,
                                    createdAt = 1_723_460_002uL,
                                    kind = 10_042u,
                                    tags = listOf(listOf("x", "caller-owned")),
                                    content = "stale replacement",
                                    expectedBase = baseId,
                                ),
                            durability = Durability.Durable,
                            routing = WriteRouting.AuthorOutbox,
                        ),
                    )
                assertEquals(
                    listOf(WriteStatus.ReplaceableConflict(baseId, exactId)),
                    withTimeout(5_000) { stale.status.take(1).toList() },
                )
                assertEquals(
                    ReceiptReattachment.NotFound,
                    engine.reattachReceipt(stale.id),
                    "a stale guard must leave no durable receipt",
                )

                val first =
                    engine.publish(
                        WriteIntent(
                            payload =
                                WritePayload.UnsignedReplaceableEdit(
                                    pubkey = account.publicKey,
                                    createdAt = 1_723_460_003uL,
                                    kind = 10_043u,
                                    tags = emptyList(),
                                    content = "first value",
                                    expectedBase = null,
                                ),
                            durability = Durability.Durable,
                            routing = WriteRouting.AuthorOutbox,
                        ),
                    )
                assertEquals(64, signedId(first).length)
            }
        }
}
