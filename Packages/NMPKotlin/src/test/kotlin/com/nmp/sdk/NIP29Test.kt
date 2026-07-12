package com.nmp.sdk

import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeoutOrNull
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertFalse
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

    // groupSendIntent / publishComposed (#115)

    @Test
    fun groupSendIntentComposesAKindBlindGroupSend() {
        // Construction-only: proves the wrapper builds and is callable with
        // arbitrary/unusual kinds -- the live round-trip (reaches only the
        // pinned host, frozen template, read-back) is the Rust falsifier
        // (pinned_host_write.rs), never re-driven against a live relay here.
        groupSendIntent(
            host = "wss://group-host.example.com",
            groupId = "group-a",
            authorPubkey = "a".repeat(64),
            createdAt = 1uL,
            kind = 9999u,
            content = "hi",
        )
    }

    @Test
    fun groupSendIntentRejectsAReservedExtraTag() {
        val error =
            assertThrows(NMPError.ReservedGroupTag::class.java) {
                groupSendIntent(
                    host = "wss://group-host.example.com",
                    groupId = "group-a",
                    authorPubkey = "a".repeat(64),
                    createdAt = 1uL,
                    kind = 9u,
                    content = "hi",
                    extraTags = listOf(listOf("h", "sneaky")),
                )
            }
        assertEquals("h", error.got)
    }

    @Test
    fun groupSendIntentComposesFromCouriedRows() {
        // recentRows couriers delivered rows exactly as a live
        // groupContentDemand read would render them -- proves the wrapper
        // plumbs Row through to the FFI boundary without the caller ever
        // touching an FfiRow.
        val recent =
            Row(
                id = "1".repeat(64),
                pubkey = "a".repeat(64),
                createdAt = 100uL,
                kind = 9u,
                tags = listOf(listOf("h", "group-a")),
                content = "earlier",
                sig = "sig",
                sources = emptyList(),
            )
        groupSendIntent(
            host = "wss://group-host.example.com",
            groupId = "group-a",
            authorPubkey = "a".repeat(64),
            createdAt = 200uL,
            kind = 9u,
            content = "hi",
            recentRows = listOf(recent),
        )
    }

    /** Take-once (falsifier 10), no live relay needed: mirrors the Rust FFI
     * falsifier (`ffi_publish_composed_takes_the_intent_exactly_once`) --
     * no signer is ever attached, so the first `publishComposed` settles
     * into the retained `Accepted`/`AwaitingCapability` steady state; a
     * second call on the SAME `GroupSendIntent` must throw
     * `NMPError.IntentAlreadyConsumed`. */
    @Test
    fun publishComposedTakesTheIntentExactlyOnce() {
        runBlocking {
            NMPEngine(NMPConfig()).use { engine ->
                val pubkey = "a".repeat(64)
                engine.setActiveAccount(pubkey)

                val intent =
                    groupSendIntent(
                        host = "wss://group-host.example.com",
                        groupId = "group-a",
                        authorPubkey = pubkey,
                        createdAt = 1uL,
                        kind = 9u,
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
