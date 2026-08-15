// Kotlin/JVM mirror of FollowingTests.swift: construction/mapping-level
// proofs only, no live relay needed. #1640: every pre-custody refusal is now
// a synchronous exception from `follow`/`unfollow` itself, exactly like the
// Swift suite -- there is no follow-only action/status stream to collect
// before observing it.
package com.nmp.sdk

import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeoutOrNull
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertNotNull
import org.junit.jupiter.api.Assertions.assertNull
import org.junit.jupiter.api.Assertions.assertThrows
import org.junit.jupiter.api.Test

class FollowingTest {
    companion object {
        private val TARGET = "ab".repeat(32)
    }

    @Test
    fun signedOutObservationIsUnknownAndUnavailable() =
        runBlocking {
            NMPEngine(config()).use { engine ->
                val snapshot =
                    withTimeoutOrNull(3_000) {
                        engine.observeFollowing(TARGET).first()
                    }
                assertNotNull(snapshot, "NMP must project the signed-out state without relay I/O")

                assertNull(snapshot!!.currentPubkey)
                assertEquals(TARGET, snapshot.target)
                assertEquals(FollowRelationship.Unknown, snapshot.relationship)
                assertEquals(FollowAvailability.SignedOut, snapshot.availability)
                assertNull(snapshot.baseEventId)
            }
        }

    /** #1640: a signed-out follow is a truthful immediate refusal -- there is
     * no receipt, and therefore no stream, to observe it through. */
    @Test
    fun signedOutFollowRefusesBeforeReceiptCustody() {
        NMPEngine(config()).use { engine ->
            val error = assertThrows(FollowActionError::class.java) { engine.follow(TARGET) }
            assertEquals(FollowActionError.SignedOut, error)
        }
    }

    /** #1640: an unparseable target refuses synchronously, exactly like every
     * other pre-custody refusal -- there is no separate typed-action-state
     * channel for it to hide in. */
    @Test
    fun invalidTargetRefusesBeforeReceiptCustody() {
        NMPEngine(config()).use { engine ->
            val error =
                assertThrows(FollowActionError::class.java) { engine.follow("not-a-pubkey") }
            assertEquals(FollowActionError.InvalidTarget("not-a-pubkey"), error)
        }
    }

    @Test
    fun providerlessFollowRefusesBeforeReceiptCustody() {
        NMPEngine(NMPConfig()).use { engine ->
            val error = assertThrows(FollowActionError::class.java) { engine.follow(TARGET) }
            assertEquals(FollowActionError.AutomaticRoutingUnavailable, error)
        }
    }

    private fun config(): NMPConfig =
        NMPConfig(outboxRouting = OutboxRoutingConfig(indexers = listOf("wss://indexer.example")))
}
