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

    @Test
    fun activeAccountDemandTargetsKind10009() {
        val demand = activeAccountDemand()
        assertEquals(listOf<UShort>(10009u), demand.selection.kinds)
    }

    /** Browsing a group takes a host the app explicitly supplies; the parsed
     * value never becomes routing authority on its own.
     *
     * #858's Kotlin falsifier too: the selected [SimpleGroupEntry] feeds
     * NIP-29's host-pinned discovery constructor directly, with no
     * NIP-29-owned copy of the NIP-51 value in between. */
    @Test
    fun groupBrowsingStillTakesAnExplicitlySuppliedHost() {
        val list = parseSimpleGroupsListTolerant(fabricatedRow(10009u))
        val selected = list.items[0]
        val demand = groupDiscoveryDemand(selected.hostRelay)
        assertEquals(listOf<UShort>(39000u), demand.selection.kinds)

        assertEquals("group-a", selected.groupId)
    }
}
