// Kotlin/JVM mirror of FollowingTests.swift: construction/mapping-level
// proofs only, no live relay needed -- signed-out projection and typed
// action failures are both synchronous-from-the-engine's-perspective
// states, exactly like the Swift suite.
package com.nmp.sdk

import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.take
import kotlinx.coroutines.flow.toList
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeoutOrNull
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertNotNull
import org.junit.jupiter.api.Assertions.assertNull
import org.junit.jupiter.api.Test

class FollowingTest {
    companion object {
        private val TARGET = "ab".repeat(32)
    }

    @Test
    fun signedOutObservationIsUnknownAndUnavailable() =
        runBlocking {
            NMPEngine(NMPConfig()).use { engine ->
                val snapshot =
                    withTimeoutOrNull(3_000) {
                        engine.observeFollowing(TARGET).first()
                    }
                assertNotNull(snapshot, "NMP must project the signed-out state without relay I/O")

                assertNull(snapshot!!.activePubkey)
                assertEquals(TARGET, snapshot.target)
                assertEquals(FollowRelationship.Unknown, snapshot.relationship)
                assertEquals(FollowAvailability.SignedOut, snapshot.availability)
                assertNull(snapshot.baseEventId)
            }
        }

    @Test
    fun followIsAnNMPActionWithTypedSignedOutFailure() =
        runBlocking {
            NMPEngine(NMPConfig()).use { engine ->
                val action = engine.follow(TARGET)
                val statuses = withTimeoutOrNull(3_000) { action.status.take(2).toList() }

                assertEquals(
                    listOf(
                        FollowActionStatus.Acquiring,
                        FollowActionStatus.Failed(FollowActionFailure.SignedOut),
                    ),
                    statuses,
                )
            }
        }

    @Test
    fun invalidTargetIsTypedActionStateNotANativeException() =
        runBlocking {
            NMPEngine(NMPConfig()).use { engine ->
                val action = engine.follow("not-a-pubkey")
                val statuses = withTimeoutOrNull(3_000) { action.status.take(1).toList() }

                assertEquals(
                    listOf(
                        FollowActionStatus.Failed(FollowActionFailure.InvalidTarget("not-a-pubkey")),
                    ),
                    statuses,
                )
            }
        }
}
