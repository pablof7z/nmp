package com.nmp.sdk

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith

class NIP29Test {
    @Test
    fun groupDiscoveryDemandPinsTheParsedHost() {
        val demand = groupDiscoveryDemand("wss://host-1.example.com")
        assertEquals(listOf(39000.toUShort()), demand.selection.kinds)
        assertEquals(
            NMPSourceAuthority.Pinned(setOf("wss://host-1.example.com")),
            demand.source,
        )
    }

    @Test
    fun groupDiscoveryDemandRejectsAnUnparseableHost() {
        val error = assertFailsWith<NMPError.InvalidRelayUrl> {
            groupDiscoveryDemand("not-a-url")
        }
        assertEquals("not-a-url", error.got)
    }
}
