package com.nmp.sdk

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertIs

class ContentTest {
    private val npub = "npub14f8usejl26twx0dhuxjh9cas7keav9vr0v8nvtwtrjqx3vycc76qqh9nsy"
    private val note = "note1m99r7nwc0wdrkzldrqan96gklg5usqspq7z9696j6unf0ljnpxjspqfw99"

    @Test
    fun parserKeepsOccurrenceAndNormalizesProfile() {
        val document = parseNostrContent("hello nostr:$npub")
        assertEquals(1, document.references.size)
        val occurrence = document.references.single()
        assertEquals(NostrReferencePlacement.Inline, occurrence.placement)
        assertIs<NostrReferenceTarget.Profile>(occurrence.target)
    }

    @Test
    fun standaloneReferencePreservesPlacement() {
        assertEquals(
            NostrReferencePlacement.Standalone,
            parseNostrContent("nostr:$npub").references.single().placement,
        )
    }

    @Test
    fun parsingAndPlanningAreEngineFree() {
        // #680 removed the native-task census: parsing content and lowering
        // its references to demand plans are pure, engine-free value
        // operations, so there is no longer a census to read before/after.
        // The surviving invariant is that they succeed without any engine.
        val document = parseNostrContent("nostr:$npub nostr:$note")
        val plans = document.references.map { referenceDemandPlan(it.target) }
        assertEquals(document.references.size, plans.size)
    }
}
