package com.nmp.sdk

import kotlinx.coroutines.flow.take
import kotlinx.coroutines.flow.toList
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test

/** #591: crash-safe publish correlation exercised through the public
 * Kotlin SDK -- a caller-generated token reattaches an existing obligation
 * instead of enqueuing a second write, and `reattachReceipt(correlation:)`
 * recovers a receipt the caller never learned the numeric id of. */
class CorrelationTest {
    private val author = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"

    @Test
    fun doubleSubmitWithTheSameTokenReattachesInsteadOfEnqueuingASecondWrite() =
        runBlocking {
            NMPEngine(
                NMPConfig(nip65 = NIP65Config(indexerRelays = listOf("wss://indexer.example"))),
            ).use { engine ->
                engine.setActiveAccount(author)
                val token = "kotlin-sdk-correlation-token"

                val first =
                    engine.publish(
                        WriteIntent(
                            payload =
                                WritePayload.Event(
                                    kind = 1u.toUShort(),
                                    tags = emptyList(),
                                    content = "first draft",
                                    createdAt = 1_723_456_800uL,
                                ),
                            routing = WriteRouting.Auto,
                            correlation = token,
                        ),
                    )
                // Acceptance is `publish` returning this receipt, not a
                // stream item; the LIVE stream's first fact is the park.
                val firstFacts = withTimeout(5_000) { first.status.take(1).toList() }
                assertEquals(
                    listOf(WriteFact.Signing(SigningState.AwaitingSigner(author))),
                    firstFacts,
                )

                // A re-composed draft -- different timestamp/content -- under
                // the SAME token must resolve to the SAME receipt id, never a
                // new one.
                val second =
                    engine.publish(
                        WriteIntent(
                            payload =
                                WritePayload.Event(
                                    kind = 1u.toUShort(),
                                    tags = emptyList(),
                                    content = "second, different draft",
                                    createdAt = 1_723_456_801uL,
                                ),
                            routing = WriteRouting.Auto,
                            correlation = token,
                        ),
                    )
                assertEquals(first.id, second.id)
                // The retry reattaches, so its stream REPLAYS the original
                // obligation's retained facts -- both parks, signer and route.
                val secondFacts = withTimeout(5_000) { second.status.take(2).toList() }
                assertEquals(
                    listOf(
                        WriteFact.Signing(SigningState.AwaitingSigner(author)),
                        // The park names nobody: this write is held on its SIGNER,
                        // so no route lookup is outstanding to name. The reason
                        // it is stuck is the signing fact above it.
                        WriteFact.Destinations(emptyList(), false, emptyList()),
                    ),
                    secondFacts,
                    "the retry's stream must replay the ORIGINAL obligation's facts",
                )
            }
        }

    @Test
    fun reattachByCorrelationRecoversAReceiptTheCallerNeverLearnedTheIdOf() =
        runBlocking {
            NMPEngine(
                NMPConfig(nip65 = NIP65Config(indexerRelays = listOf("wss://indexer.example"))),
            ).use { engine ->
                engine.setActiveAccount(author)
                val token = "kotlin-sdk-reattach-by-correlation"

                val receipt =
                    engine.publish(
                        WriteIntent(
                            payload =
                                WritePayload.Event(
                                    kind = 1u.toUShort(),
                                    tags = emptyList(),
                                    content = "reattach by correlation",
                                    createdAt = 1_723_456_900uL,
                                ),
                            routing = WriteRouting.Auto,
                            correlation = token,
                        ),
                    )
                withTimeout(5_000) { receipt.status.take(1).toList() }

                // Simulate the "app forgot the numeric id" scenario: reattach
                // using only the token it minted itself.
                val reattachment = engine.reattachReceipt(token)
                assertTrue(reattachment is ReceiptReattachment.Attached)
                val replay = (reattachment as ReceiptReattachment.Attached).receipt
                val replayFacts = withTimeout(5_000) { replay.status.take(2).toList() }
                assertEquals(
                    listOf(
                        WriteFact.Signing(SigningState.AwaitingSigner(author)),
                        // The park names nobody: this write is held on its SIGNER,
                        // so no route lookup is outstanding to name. The reason
                        // it is stuck is the signing fact above it.
                        WriteFact.Destinations(emptyList(), false, emptyList()),
                    ),
                    replayFacts,
                )

                // An unknown token is a distinct, typed absence.
                assertEquals(
                    ReceiptReattachment.NotFound,
                    engine.reattachReceipt("never-seen-token"),
                )
            }
        }

    @Test
    fun malformedCorrelationTokenOnPublishThrowsSynchronously() =
        runBlocking {
            NMPEngine(NMPConfig()).use { engine ->
                engine.setActiveAccount(author)
                val error =
                    org.junit.jupiter.api.Assertions.assertThrows(
                        NMPError.InvalidCorrelationToken::class.java,
                    ) {
                        engine.publish(
                            WriteIntent(
                                payload =
                                    WritePayload.Event(
                                        kind = 1u.toUShort(),
                                        tags = emptyList(),
                                        content = "malformed correlation token",
                                        createdAt = 1_723_457_000uL,
                                    ),
                                    routing = WriteRouting.Auto,
                                correlation = "",
                            ),
                        )
                    }
                assertEquals("", error.got)
            }
        }

    @Test
    fun anUnknownCorrelationTokenReportsNotFound() =
        runBlocking {
            NMPEngine(NMPConfig()).use { engine ->
                assertEquals(
                    ReceiptReattachment.NotFound,
                    engine.reattachReceipt("never-seen-token"),
                )
            }
        }
}
