package com.nmp.sdk

import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test

class NIP51Test {
    private fun fabricatedRow(kind: UShort): Row =
        Row(
            id = "caller-chosen-id",
            pubkey = "caller-chosen-pubkey",
            createdAt = 1uL,
            kind = kind,
            tags =
                listOf(
                    listOf("group", "group-a", "wss://relay-a.example.com", "Group A"),
                    listOf("group", "missing-relay"),
                    listOf("r", "wss://relay-in-use.example.com"),
                ),
            content = "encrypted-private-items",
            sig = "caller-chosen-signature",
            sources = emptyList(),
        )

    /** #863: a row the app fabricated -- wrong kind, invented signature, no
     * relay sources -- still parses, still reports its evidence, and still
     * yields nothing but data. */
    @Test
    fun tolerantParserPreservesEvidenceForFabricatedWrongKindRow() {
        val list = parseSimpleGroupsListTolerant(fabricatedRow(1u))
        assertEquals(1, list.items.size)
        assertEquals("group-a", list.items[0].groupId)
        assertEquals("wss://relay-a.example.com", list.items[0].hostRelay)
        assertEquals("Group A", list.items[0].name)
        assertEquals(listOf("wss://relay-in-use.example.com"), list.relaysInUse)
        assertEquals(1uL, list.malformedItemCount)
        assertTrue(list.hasPrivateContent)

        // The kind:10009 spelling buys the value nothing extra.
        assertEquals(list, parseSimpleGroupsListTolerant(fabricatedRow(10009u)))
    }

    /** Browsing a group takes a host the app explicitly supplies; the parsed
     * value never becomes routing authority on its own. */
    @Test
    fun groupBrowsingStillTakesAnExplicitlySuppliedHost() {
        val list = parseSimpleGroupsListTolerant(fabricatedRow(10009u))
        val demand = groupDiscoveryDemand(list.items[0].hostRelay)
        assertEquals(listOf<UShort>(39000u), demand.selection.kinds)
    }
}
