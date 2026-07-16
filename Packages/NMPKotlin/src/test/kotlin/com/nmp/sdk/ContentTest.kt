package com.nmp.sdk

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertIs
import kotlin.test.assertNull
import kotlin.test.assertTrue

class ContentTest {
    private val npub = "npub14f8usejl26twx0dhuxjh9cas7keav9vr0v8nvtwtrjqx3vycc76qqh9nsy"
    private val note = "note1m99r7nwc0wdrkzldrqan96gklg5usqspq7z9696j6unf0ljnpxjspqfw99"
    private val naddr =
        "naddr1qqxnzd3exgersv33xymnsve3qgs8suecw4luyht9ekff89x4uacneapk8r5dyk0gmn6uwwurf6u9rusrqsqqqa282m3gxt"
    private val pubkey =
        "aa4fc8665f5696e33db7e1a572e3b0f5b3d615837b0f362dcb1c8068b098c7b4"

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
    fun parsingAndPlanningOpenNoNativeTasks() {
        NMPEngine(NMPConfig()).use { engine ->
            val baseline = engine.nativeTaskCensus()
            val document = parseNostrContent("nostr:$npub nostr:$note")
            document.references.forEach { referenceDemandPlan(it.target) }

            val after = engine.nativeTaskCensus()
            assertEquals(baseline.admitted, after.admitted)
            assertEquals(baseline.running, after.running)
        }
    }

    @Test
    fun sharedPublicNip19FixturesLowerToExactOrdinaryDemands() {
        val plan =
            referenceDemandPlan(
                NostrReferenceTarget.Profile(pubkey),
            )
        assertIs<NMPSourceAuthority.AuthorOutboxes>(plan.canonical.source)
        assertEquals(listOf(0.toUShort()), plan.canonical.selection.kinds)
        assertEquals(setOf(pubkey), plan.canonical.selection.authors.literalValues())
        assertEquals(1U, plan.canonical.selection.limit)
        assertTrue(plan.helpers.isEmpty())
        assertEquals(0U, plan.discardedRelayHints)

        val event =
            referenceDemandPlan(
                parseNostrContent("nostr:$note").references.single().target,
            )
        assertIs<NMPSourceAuthority.Public>(event.canonical.source)
        assertNull(event.canonical.selection.kinds)
        assertNull(event.canonical.selection.authors)
        assertEquals(1, event.canonical.selection.ids.literalValues().size)
        assertEquals(1U, event.canonical.selection.limit)

        val address =
            referenceDemandPlan(
                parseNostrContent("nostr:$naddr").references.single().target,
            )
        assertIs<NMPSourceAuthority.AuthorOutboxes>(address.canonical.source)
        assertEquals(listOf(30_023.toUShort()), address.canonical.selection.kinds)
        assertEquals(
            setOf("787338757fc25d65cd929394d5e7713cf43638e8d259e8dcf5c73b834eb851f2"),
            address.canonical.selection.authors.literalValues(),
        )
        assertEquals(setOf("1692282117831"), address.canonical.selection.tags['d'].literalValues())
    }

    @Test
    fun eventHintsNeverConstrainCanonicalSelectionOrTargetIdentity() {
        val target =
            NostrReferenceTarget.Event(
                id = "1".repeat(64),
                authorHint = "2".repeat(64),
                kindHint = 30_023.toUShort(),
            )
        val plan = referenceDemandPlan(target)

        assertEquals("event:${"1".repeat(64)}", target.key)
        assertNull(plan.canonical.selection.authors)
        assertNull(plan.canonical.selection.kinds)
        assertEquals(setOf("1".repeat(64)), plan.canonical.selection.ids.literalValues())
        assertEquals(1, plan.helpers.size)
        assertIs<NMPSourceAuthority.AuthorOutboxes>(plan.helpers.single().source)
        assertEquals(setOf("2".repeat(64)), plan.helpers.single().selection.authors.literalValues())
        assertNull(plan.helpers.single().selection.kinds)
    }

    @Test
    fun relayHintsStayBoundedSeparateAndExplicitAboutDiscardedInputs() {
        val plan =
            referenceDemandPlan(
                NostrReferenceTarget.Event(
                    id = "1".repeat(64),
                    relayHints =
                        listOf(
                            "wss://RELAY.EXAMPLE.com",
                            "wss://relay.example.com/",
                            "not a relay",
                            "ws://127.0.0.1:7777",
                        ),
                ),
            )

        assertIs<NMPSourceAuthority.Public>(plan.canonical.source)
        val helper = plan.helpers.single()
        val pinned = assertIs<NMPSourceAuthority.Pinned>(helper.source)
        assertEquals(setOf("wss://relay.example.com"), pinned.relays)
        assertEquals(plan.canonical.selection, helper.selection)
        assertEquals(2U, plan.discardedRelayHints)
    }

    @Test
    fun malformedIdentityCannotProduceADemand() {
        assertFailsWith<NMPError.InvalidPublicKey> {
            referenceDemandPlan(NostrReferenceTarget.Profile("not-a-pubkey"))
        }
        assertFailsWith<NMPError.InvalidEventId> {
            referenceDemandPlan(NostrReferenceTarget.Event("not-an-event"))
        }
    }

    @Test
    fun secretKeyEntityRemainsLiteralAndNeverBecomesAReference() {
        val nsec = "nsec1j4c6269y9w0q2er2xjw8sv2ehyrtfxq3jwgdlxj6qfn8z4gjsq5qfvfk99"
        val document = parseNostrContent("nostr:$nsec")
        assertTrue(document.references.isEmpty())
        val visibleText =
            document.blocks
                .flatMap { it.inlines }
                .filterIsInstance<NostrContentInline.Text>()
                .joinToString(separator = "") { it.text }
        assertTrue(visibleText.contains(nsec))
    }
}

private fun NMPBinding?.literalValues(): Set<String> =
    assertIs<NMPBinding.Literal>(this).values
