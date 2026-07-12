package com.nmp.sdk

import kotlinx.coroutines.delay
import kotlinx.coroutines.runBlocking
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertNotNull

class ContentSessionTest {
    private val npub = "npub180cvv07tjdrrgpa0j7j7tmnyl2yr6yr7l8j4s3evf6u64th6gkwsyjh6w6"

    @Test
    fun duplicateOccurrencesShareOneTargetAndLastCloseWithdraws() =
        runBlocking {
            NMPEngine(NMPConfig()).use { engine ->
                val session =
                    NMPContentClient(engine).session(
                        "nostr:$npub and nostr:$npub",
                        this,
                        policy = NostrContentPolicy(releaseGraceMilliseconds = 0),
                    )
                val references = session.snapshot.value.document.references
                val first = assertNotNull(session.claim(references[0].id))
                val second = assertNotNull(session.claim(references[1].id))
                assertEquals(1, session.snapshot.value.activeReferenceCount)
                first.close()
                delay(20)
                assertEquals(1, session.snapshot.value.activeReferenceCount)
                second.close()
                repeat(100) {
                    if (session.snapshot.value.activeReferenceCount == 0) return@repeat
                    delay(10)
                }
                assertEquals(0, session.snapshot.value.activeReferenceCount)
                session.close()
            }
        }
}
