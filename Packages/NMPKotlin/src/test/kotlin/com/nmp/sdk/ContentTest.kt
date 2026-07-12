package com.nmp.sdk

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertIs
import kotlin.test.assertTrue

class ContentTest {
    private val npub = "npub14f8usejl26twx0dhuxjh9cas7keav9vr0v8nvtwtrjqx3vycc76qqh9nsy"

    @Test
    fun parserKeepsOccurrenceAndNormalizesProfile() {
        val document = parseNostrContent("hello nostr:$npub")
        assertEquals(1, document.references.size)
        val occurrence = document.references.single()
        assertEquals(NostrReferencePlacement.Inline, occurrence.placement)
        val target = assertIs<NostrReferenceTarget.Profile>(occurrence.target)
        assertEquals(
            "aa4fc8665f5696e33db7e1a572e3b0f5b3d615837b0f362dcb1c8068b098c7b4",
            target.pubkey,
        )
    }

    @Test
    fun standaloneReferencePreservesPlacement() {
        assertEquals(
            NostrReferencePlacement.Standalone,
            parseNostrContent("nostr:$npub").references.single().placement,
        )
    }

    @Test
    fun profilePlanIsOrdinaryNmpDemand() {
        val plan =
            referenceDemandPlan(
                NostrReferenceTarget.Profile(
                    "aa4fc8665f5696e33db7e1a572e3b0f5b3d615837b0f362dcb1c8068b098c7b4",
                ),
            )
        assertIs<NMPSourceAuthority.AuthorOutboxes>(plan.canonical.source)
        assertEquals(listOf(0.toUShort()), plan.canonical.selection.kinds)
        assertTrue(plan.helpers.isEmpty())
    }
}
