package com.nmp.sdk

import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Assertions.assertThrows
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
            signature = RowSignature.Signed("caller-chosen-signature"),
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
     * #858's Kotlin falsifier too, updated for #1033: the selected
     * [SimpleGroupEntry] feeds NIP-29's host-scoped door
     * ([NMPRelayScope.on]/[NMPRelayScope.group]) directly, with no
     * NIP-29-owned copy of the NIP-51 value in between. */
    @Test
    fun groupBrowsingStillTakesAnExplicitlySuppliedHost() {
        val list = parseSimpleGroupsListTolerant(fabricatedRow(10009u))
        val selected = list.items[0]
        val scope = NMPRelayScope.on(listOf(selected.hostRelay))
        val group = scope.group(selected.groupId)
        val query = group.read(NMPFilter(kinds = listOf(9u)))
        assertEquals(1, query.branches.size)
        assertEquals(listOf<UShort>(9u), query.branches[0].selection.kinds)

        assertEquals("group-a", selected.groupId)
    }

    /** #1245: this test used to read kind 39000 through the content door and
     * assert the request was built faithfully. No 39000 event carries the
     * group-context row, so that request could never have matched anything --
     * it is refused now, and the group's own metadata is read through the
     * records observation instead. */
    @Test
    fun theGroupsOwnRecordsAreNotReachableThroughTheContentDoor() {
        val list = parseSimpleGroupsListTolerant(fabricatedRow(10009u))
        val selected = list.items[0]
        val group = NMPRelayScope.on(listOf(selected.hostRelay)).group(selected.groupId)
        val error =
            assertThrows(NMPError.GroupRecordsNotContextScoped::class.java) {
                group.read(NMPFilter(kinds = listOf(39000u)))
            }
        assertEquals(listOf<UShort>(39000u), error.kinds)
    }
}
