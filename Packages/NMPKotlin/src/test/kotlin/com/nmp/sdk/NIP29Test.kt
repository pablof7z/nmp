package com.nmp.sdk

import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertFalse
import org.junit.jupiter.api.Assertions.assertThrows
import org.junit.jupiter.api.Test

// The read-only NIP-29 host-browser projection (#108) -- construction/
// mapping tests only. No network: these are pure Demand constructors and
// a pure decode.
class NIP29Test {
    @Test
    fun activeAccountDemandTargetsKind10009() {
        val demand = activeAccountDemand()
        assertEquals(listOf<UShort>(10009u), demand.selection.kinds)
    }

    @Test
    fun groupDiscoveryDemandPinsTheParsedHost() {
        val demand = groupDiscoveryDemand("wss://host-1.example.com")
        assertEquals(listOf<UShort>(39000u), demand.selection.kinds)
        val source = demand.source as NMPSourceAuthority.Pinned
        assertEquals(setOf("wss://host-1.example.com"), source.relays)
    }

    @Test
    fun groupDiscoveryDemandRejectsAnUnparseableHost() {
        val error =
            assertThrows(NMPError.InvalidRelayUrl::class.java) {
                groupDiscoveryDemand("not-a-url")
            }
        assertEquals("not-a-url", error.got)
    }

    @Test
    fun groupContentDemandScopesByHTag() {
        val demand = groupContentDemand("wss://host-1.example.com", "group-a")
        assertEquals(listOf<UShort>(9u, 30315u), demand.selection.kinds)
    }

    @Test
    fun decodeRememberedGroupsComposesAKind10009Row() {
        val row =
            Row(
                id = "id",
                pubkey = "pubkey",
                createdAt = 1uL,
                kind = 10009u,
                tags = listOf(listOf("group", "group-a", "wss://relay-a.example.com", "Group A")),
                content = "",
                sig = "sig",
                sources = emptyList(),
            )
        val remembered = decodeRememberedGroups(row)
        assertEquals(1, remembered.groups.size)
        assertEquals("group-a", remembered.groups[0].groupId)
        assertEquals("wss://relay-a.example.com", remembered.groups[0].host)
        assertEquals("Group A", remembered.groups[0].name)
        assertFalse(remembered.hasPrivateContent)
    }
}
