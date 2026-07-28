package com.nmp.sdk

import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertThrows
import org.junit.jupiter.api.Test

class ReceiptSetTest {
    @Test
    fun exactCapacityAndTaggedAbsenceCrossTheKotlinSurface() =
        runBlocking {
            NMPEngine(NMPConfig()).use { engine ->
                assertEquals(32uL, engine.receiptSetCapacity)
                val exact =
                    (1uL..engine.receiptSetCapacity).map(ReceiptSetIdentity::Id)
                assertEquals(
                    ReceiptSetEvent.NotFound(ReceiptSetIdentity.Id(1uL)),
                    engine.observeReceipts(exact).first(),
                )

                val plusOne =
                    (1uL..engine.receiptSetCapacity + 1uL).map(ReceiptSetIdentity::Id)
                assertEquals(
                    NMPReceiptSetError.CapacityExceeded(32uL, 33uL),
                    assertThrows(NMPReceiptSetError.CapacityExceeded::class.java) {
                        engine.observeReceipts(plusOne)
                    },
                )
            }
        }
}
