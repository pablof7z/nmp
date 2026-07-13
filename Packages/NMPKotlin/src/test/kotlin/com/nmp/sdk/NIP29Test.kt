package com.nmp.sdk

import kotlinx.coroutines.async
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeoutOrNull
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertFalse
import org.junit.jupiter.api.Assertions.assertNotNull
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Assertions.assertThrows
import org.junit.jupiter.api.Assertions.fail
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

    // groupMessageIntent / publishComposed (#156)

    @Test
    fun groupMessageIntentRequiresAnActiveAccount() {
        NMPEngine(NMPConfig()).use { engine ->
            assertThrows(NMPError.NoActiveAccount::class.java) {
                engine.groupMessageIntent(
                    host = "wss://group-host.example.com",
                    groupId = "group-a",
                    content = "hello",
                )
            }
        }
    }

    @Test
    fun groupMessageIntentMaterializesTheCanonicalSemanticTemplate() {
        runBlocking {
            NMPEngine(NMPConfig()).use { engine ->
                val author = engine.addAccount("0".repeat(63) + "1")
                engine.setActiveAccount(author)
                val host = "wss://group-host.example.com"
                val groupId = "group-a"
                val first = "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d"
                val second = "7e7e9c42a91bfef19fa929e5fda1b72e0ebc1a4c1141673e2794234d86addf4e"
                val parentId = "1".repeat(64)
                val previousId = "2".repeat(64)
                val rowDeferred =
                    async {
                        withTimeoutOrNull(5_000) {
                            engine.observe(groupContentDemand(host, groupId))
                                .first { it.rows.isNotEmpty() }
                                .rows
                                .first()
                        }
                    }
                val recent =
                    Row(
                        id = previousId,
                        pubkey = author,
                        createdAt = 100uL,
                        kind = 9u,
                        tags = listOf(listOf("h", groupId)),
                        content = "earlier",
                        sig = "sig",
                        sources = emptyList(),
                    )

                val intent =
                    engine.groupMessageIntent(
                        host = host,
                        groupId = groupId,
                        content = "hello",
                        recipients = listOf(first, first, second),
                        reply = GroupReplyParent(parentId, first),
                        recentRows = listOf(recent),
                    )
                val receipt = engine.publishComposed(intent)
                assertEquals(WriteStatus.Accepted, withTimeoutOrNull(5_000) { receipt.status.first() })

                val row = rowDeferred.await()
                assertNotNull(row)
                assertEquals(author, row?.pubkey)
                assertEquals(9u.toUShort(), row?.kind)
                assertTrue((row?.createdAt ?: 0uL) > 1_700_000_000uL)
                assertEquals(
                    "nostr:npub180cvv07tjdrrgpa0j7j7tmnyl2yr6yr7l8j4s3evf6u64th6gkwsyjh6w6 " +
                        "nostr:npub10elfcs4fr0l0r8af98jlmgdh9c8tcxjvz9qkw038js35mp4dma8qzvjptg hello",
                    row?.content,
                )
                assertEquals(
                    listOf(
                        listOf("p", first),
                        listOf("p", second),
                        listOf("e", parentId, "", "reply", first),
                        listOf("h", groupId),
                        listOf("previous", "22222222"),
                    ),
                    row?.tags,
                )
            }
        }
    }

    @Test
    fun groupMessageIntentRejectsMalformedTypedRecipients() {
        NMPEngine(NMPConfig()).use { engine ->
            engine.setActiveAccount("a".repeat(64))
            val error =
                assertThrows(NMPError.InvalidPublicKey::class.java) {
                    engine.groupMessageIntent(
                        host = "wss://group-host.example.com",
                        groupId = "group-a",
                        content = "hello",
                        recipients = listOf("not-a-pubkey"),
                    )
                }
            assertEquals("not-a-pubkey", error.got)
        }
    }

    @Test
    fun publishComposedTakesTheIntentExactlyOnce() {
        runBlocking {
            NMPEngine(NMPConfig()).use { engine ->
                val pubkey = "a".repeat(64)
                engine.setActiveAccount(pubkey)

                val intent =
                    engine.groupMessageIntent(
                        host = "wss://group-host.example.com",
                        groupId = "group-a",
                        content = "hi",
                    )

                val receipt = engine.publishComposed(intent)
                val first = withTimeoutOrNull(5_000) { receipt.status.first() }
                assertEquals(WriteStatus.Accepted, first)

                try {
                    engine.publishComposed(intent)
                    fail<Unit>("expected the second publishComposed call to throw")
                } catch (e: NMPError.IntentAlreadyConsumed) {
                    // expected
                }
            }
        }
    }
}
