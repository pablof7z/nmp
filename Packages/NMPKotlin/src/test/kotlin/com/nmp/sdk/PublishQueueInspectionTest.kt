package com.nmp.sdk

import org.junit.jupiter.api.Assertions.assertThrows
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test

class PublishQueueInspectionTest {
    @Test
    fun boundedQueueAndExactEventDoorsCrossTheKotlinSurface() {
        NMPEngine(NMPConfig()).use { engine ->
            assertTrue(engine.publishQueue(limit = UByte.MAX_VALUE).isEmpty())
            assertTrue(
                engine.publishQueueForEvent(
                    eventId = "0".repeat(64),
                    limit = UByte.MAX_VALUE,
                ).isEmpty(),
            )

            assertThrows(NMPPublishQueueError.InvalidEventId::class.java) {
                engine.publishQueueForEvent(
                    eventId = "not-an-event-id",
                    limit = UByte.MAX_VALUE,
                )
            }
        }
    }
}
