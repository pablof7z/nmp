// The native NIP-29 relay-scope/group/predicate projection (#1033). Mirrors
// NIP29Tests.swift.

package com.nmp.sdk

import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertNull
import org.junit.jupiter.api.Assertions.assertThrows
import org.junit.jupiter.api.Test
import kotlin.random.Random

class NIP29Test {
    private fun host(n: Int): String = "wss://host-$n.example.com"

    private fun randomPubkeyHex(): String {
        val alphabet = "0123456789abcdef"
        return (0 until 64).map { alphabet[Random.nextInt(alphabet.length)] }.joinToString("")
    }

    @Test
    fun onRejectsAnEmptyRelaySet() {
        val error = assertThrows(NMPError.EmptyRelayScope::class.java) { NMPRelayScope.on(emptyList()) }
        assertEquals(NMPError.EmptyRelayScope, error)
    }

    @Test
    fun onRejectsAnUnparseableHost() {
        val error =
            assertThrows(NMPError.InvalidRelayUrl::class.java) { NMPRelayScope.on(listOf("not-a-url")) }
        assertEquals("not-a-url", error.got)
    }

    /** A multi-host group read is ONE live query with one complete branch
     * per host, each pinned to that host alone and scoped by `#h`. */
    @Test
    fun groupReadIsOneBranchPerHostPinnedToThatHost() {
        val scope = NMPRelayScope.on(listOf(host(1), host(2)))
        val group = scope.group("photographers")

        val query = group.read(NMPFilter())
        assertEquals(2, query.branches.size)
        query.branches.zip(listOf(host(1), host(2))).forEach { (branch, expectedHost) ->
            val source = branch.source
            check(source is NMPSourceAuthority.Pinned) { "expected Pinned, got $source" }
            assertEquals(setOf(expectedHost), source.relays)
            assertEquals(NMPAccessContext.Public, branch.access)
            val hBinding = branch.selection.tags['h']
            check(hBinding is NMPBinding.Literal) { "expected an h tag literal binding" }
            assertEquals(setOf("photographers"), hBinding.values)
        }
        assertNull(query.aggregateResultLimit)
    }

    /** A read selection that already constrains `#h` is refused before any
     * live query is formed -- the retained group id is the sole semantic
     * source of that row. */
    @Test
    fun groupReadNamingItsOwnHRowIsRefused() {
        val scope = NMPRelayScope.on(listOf(host(1)))
        val group = scope.group("photographers")
        val selection = NMPFilter(tags = mapOf('h' to NMPBinding.Literal(setOf("elsewhere"))))

        assertThrows(NMPError.GroupCallerSuppliedContextConstraint::class.java) {
            group.read(selection)
        }
    }

    /** The composable predicate door: union/intersect/minus fold through
     * the grammar's own set algebra, including the literal-id leaf. */
    @Test
    fun predicatesComposeIncludingTheLiteralIdLeaf() {
        val member =
            NMPGroupIds.memberListIncludes(NMPBinding.Reactive(NMPIdentityField.ActivePubkey))
        val admin =
            NMPGroupIds.adminListIncludes(NMPBinding.Reactive(NMPIdentityField.ActivePubkey))
        member.union(listOf(admin, NMPGroupIds.anyOf(NMPBinding.Literal(setOf("photographers")))))
        member.intersect(listOf(admin))
        member.minus(listOf(admin))
        NMPGroupPredicate.naming(member)
    }

    /** The #1252 capability: "every room this relay advertises" is a
     * predicate an app can phrase, with no id set of its own. */
    @Test
    fun aDirectoryNeedsNoIdSetOfItsOwn() {
        NMPGroupPredicate.all()
    }

    /** The general spelling is reachable from Kotlin, and its refusal
     * survives the boundary: a group host is authoritative for NIP-29's
     * three relay-signed records and nothing else. */
    @Test
    fun theGeneralSpellingRefusesAKindTheHostDoesNotOwn() {
        NMPGroupIds.whoseRecordMatches(NMPFilter(kinds = listOf(39_002u)))
        val error =
            assertThrows(NMPError.GroupIdSelectionNotAGroupRecordKind::class.java) {
                NMPGroupIds.whoseRecordMatches(NMPFilter(kinds = listOf(10_009u)))
            }
        assertEquals(10_009, error.kind.toInt())
    }

    /** A non-hex literal subject is a typed invalid-public-key refusal --
     * the same rule `NMPFilter.authors` carries. */
    @Test
    fun aNonHexLiteralSubjectIsATypedInvalidPublicKey() {
        val error =
            assertThrows(NMPError.InvalidPublicKey::class.java) {
                NMPGroupIds.memberListIncludes(NMPBinding.Literal(setOf("not-a-pubkey")))
            }
        assertEquals("not-a-pubkey", error.got)
    }

    /** Every named group operation reaches the one publish door, headless
     * (no relay needs to be reachable for the write to be ACCEPTED at the
     * engine's door). */
    @Test
    fun everyNamedGroupOperationReachesTheOnePublishDoor() {
        NMPEngine(NMPConfig()).use { engine ->
            val scope = NMPRelayScope.on(listOf(host(1), host(2)))
            val group = scope.group("photographers")
            val authorHex = randomPubkeyHex()
            val subjectHex = randomPubkeyHex()

            group.publish(engine, authorHex, kind = 9u, content = "first light")
            group.joinRequest(engine, authorHex, inviteCode = "code")
            group.leaveRequest(engine, authorHex)
            group.addUser(engine, authorHex, subjectHex)
            group.removeUser(engine, authorHex, subjectHex)
            group.editMetadata(engine, authorHex, name = "Photographers")
            group.deleteEvent(engine, authorHex, "09".repeat(32))
            group.createGroup(engine, authorHex)
            group.deleteGroup(engine, authorHex)
            group.createInvite(engine, authorHex, "code")
        }
    }

    /** A caller-supplied `h` tag never reaches the door: the refusal is
     * synchronous and typed, before any receipt stream exists. */
    @Test
    fun aCallerSuppliedContextNeverReachesTheDoor() {
        NMPEngine(NMPConfig()).use { engine ->
            val authorHex = randomPubkeyHex()
            val scope = NMPRelayScope.on(listOf(host(1)))
            val group = scope.group("photographers")

            assertThrows(NMPError.GroupCallerSuppliedContext::class.java) {
                group.publish(
                    engine,
                    authorHex,
                    kind = 9u,
                    tags = listOf(listOf("h", "photographers")),
                )
            }
        }
    }

    /** `deleteEvent`'s `eventId` is parsed with the same typed
     * `InvalidEventId` rule every other exact-hex event id input uses. */
    @Test
    fun deleteEventRejectsAMalformedEventId() {
        NMPEngine(NMPConfig()).use { engine ->
            val authorHex = randomPubkeyHex()
            val scope = NMPRelayScope.on(listOf(host(1)))
            val group = scope.group("photographers")

            val error =
                assertThrows(NMPError.InvalidEventId::class.java) {
                    group.deleteEvent(engine, authorHex, "not-an-event-id")
                }
            assertEquals("not-an-event-id", error.got)
        }
    }

    /** The group write status stream delivers ordinary `WriteStatus` facts,
     * with no receipt id anywhere in its shape (#1033: group writes reach
     * the engine's untracked publish door). */
    @Test
    fun groupWriteStatusStreamsOrdinaryWriteStatusFacts() =
        runBlocking {
            NMPEngine(NMPConfig()).use { engine ->
                val scope = NMPRelayScope.on(listOf(host(1)))
                val group = scope.group("photographers")
                val authorHex = randomPubkeyHex()

                val status = group.publish(engine, authorHex, kind = 9u, content = "hi")
                val first = status.status.first()
                check(first is WriteStatus.Accepted || first is WriteStatus.AwaitingCapability) {
                    "expected an early write-status fact, got $first"
                }
            }
        }
}
