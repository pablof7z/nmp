// The native NIP-29 relay-scope/group/predicate projection (#1033). Mirrors
// NIP29Tests.swift.

package com.nmp.sdk

import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertNull
import org.junit.jupiter.api.Assertions.assertThrows
import org.junit.jupiter.api.Test
import uniffi.nmp_ffi.FfiGroupAvailability
import uniffi.nmp_ffi.FfiGroupRecord
import uniffi.nmp_ffi.FfiGroupSnapshot
import uniffi.nmp_ffi.FfiListedSubject
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

    /** #1242 in Kotlin: the mint door hands back the ordinary [WriteIntent]
     * with every group decision already made and publishes nothing, and the
     * app's own crash-safe token then rides that intent through the ONE
     * general publish door (#1244). */
    @Test
    fun theMintDoorHandsBackAnIntentTheGeneralPublishDoorTakes() {
        NMPEngine(NMPConfig()).use { engine ->
            val scope = NMPRelayScope.on(listOf(host(1), host(2)))
            val group = scope.group("photographers")
            val authorHex = randomPubkeyHex()

            val intent = group.intent(authorHex, kind = 9u, content = "first light")
            assertEquals(WriteRouting.Explicit(listOf(host(1), host(2))), intent.routing)
            assertEquals(Identity.Explicit(authorHex), intent.identity)
            assertEquals(null, intent.correlation)
            val payload = intent.payload
            check(payload is WritePayload.Event) { "an unsigned draft mints an Event payload" }
            assertEquals(
                listOf(listOf("h", "photographers")),
                payload.tags.filter { it.firstOrNull() == "h" },
            )

            val receipt = publishReceipt(engine.ffi, intent.copy(correlation = "group-write-0001"))
            val reattached = reattachReceiptByCorrelation(engine.ffi, "group-write-0001")
            check(reattached is ReceiptReattachment.Attached) {
                "a correlated group write must be reattachable, got $reattached"
            }
            assertEquals(receipt.id, reattached.receipt.id)
        }
    }

    /** #1281: one intent, one `h` row per named room, minted by the door
     * with the app naming neither a relay nor an `h`. */
    @Test
    fun aSeveralGroupWriteMintsOneIntentCarryingEveryHRow() {
        val scope = NMPRelayScope.on(listOf(host(1), host(2)))
        val rooms = scope.groups(listOf("darkroom", "photographers"))
        val authorHex = randomPubkeyHex()

        val intent = rooms.intent(authorHex, kind = 30315u, tags = listOf(listOf("d", "status")))
        assertEquals(WriteRouting.Explicit(listOf(host(1), host(2))), intent.routing)
        val payload = intent.payload
        check(payload is WritePayload.Event) { "an unsigned draft mints an Event payload" }
        assertEquals(
            listOf(listOf("h", "darkroom"), listOf("h", "photographers")),
            payload.tags.filter { it.firstOrNull() == "h" },
        )
    }

    /** #1281: naming no group at all forms no write context. */
    @Test
    fun aWriteContextOverNoGroupIsNeverFormed() {
        val scope = NMPRelayScope.on(listOf(host(1)))
        val error = assertThrows(NMPError.EmptyGroupSet::class.java) { scope.groups(emptyList()) }
        assertEquals(NMPError.EmptyGroupSet, error)
    }

    /** #1283: the contextualised draft already carries the retained id, so
     * an app that signs its own bytes never spells the group id. */
    @Test
    fun theSelfSigningDoorHandsBackADraftCarryingTheRetainedId() {
        val group = NMPRelayScope.on(listOf(host(1))).group("photographers")
        val contextualised =
            group.contextualize(WritePayload.Event(kind = 9u, content = "first light"))
        check(contextualised is WritePayload.Event) { "a draft contextualises to a draft" }
        assertEquals(listOf(listOf("h", "photographers")), contextualised.tags)
        assertEquals("first light", contextualised.content)

        assertThrows(NMPError.GroupCallerSuppliedContext::class.java) {
            group.contextualize(
                WritePayload.Event(kind = 9u, tags = listOf(listOf("h", "photographers"))),
            )
        }
    }

    /** #1234 in Kotlin: the negative rests on SETTLEMENT, so the same row set
     * answers differently while one relay is still acquiring -- the
     * distinction a moderation receipt structurally cannot make. */
    @Test
    fun absenceIsClaimedOnlyOnceEveryHostHasSettled() {
        val subject = randomPubkeyHex()
        listOf(
            FfiGroupAvailability.ACQUIRING,
            FfiGroupAvailability.CACHED_ONLY,
            FfiGroupAvailability.SOURCE_UNAVAILABLE,
        ).forEach { unsettled ->
            assertEquals(
                NMPListing.Unestablished,
                snapshot(availability = unsettled).memberListing(subject),
                "$unsettled is not evidence of absence",
            )
        }
        assertEquals(
            NMPListing.Absent,
            snapshot(availability = FfiGroupAvailability.READY).memberListing(subject),
        )
    }

    /** The hole settlement alone cannot see: a READY observation that never
     * asked for the member list proves nothing about it. */
    @Test
    fun aRecordTheObservationNeverAskedForIsNeverAbsent() {
        val subject = randomPubkeyHex()
        val metadataOnly =
            snapshot(
                availability = FfiGroupAvailability.READY,
                selected = listOf(FfiGroupRecord.METADATA),
            )
        assertEquals(NMPListing.Unestablished, metadataOnly.memberListing(subject))
        assertEquals(NMPListing.Unestablished, metadataOnly.adminListing(subject))
    }

    /** Inclusion is evidence whatever the availability, role and attribution
     * intact. */
    @Test
    fun inclusionIsEvidenceBeforeAnythingSettles() {
        val subject = randomPubkeyHex()
        val listing =
            snapshot(
                members = listOf(FfiListedSubject(subject, "moderator", listOf(host(1)))),
                availability = FfiGroupAvailability.ACQUIRING,
            )
                .memberListing(subject)
        check(listing is NMPListing.Named) { "inclusion is evidence, got $listing" }
        assertEquals(1, listing.entries.size)
        assertEquals("moderator", listing.entries[0].role)
        assertEquals(listOf(host(1)), listing.entries[0].hosts)
    }

    /** A malformed subject throws rather than reading as a settled absence. */
    @Test
    fun aMalformedSubjectThrowsRatherThanReadingAsAbsent() {
        assertThrows(NMPError.InvalidPublicKey::class.java) {
            snapshot(availability = FfiGroupAvailability.READY).memberListing("not-a-pubkey")
        }
    }

    private fun snapshot(
        members: List<FfiListedSubject> = emptyList(),
        availability: FfiGroupAvailability = FfiGroupAvailability.ACQUIRING,
        selected: List<FfiGroupRecord> = listOf(FfiGroupRecord.MEMBERS),
    ): NMPGroupSnapshot =
        NMPGroupSnapshot.from(
            FfiGroupSnapshot(
                id = "photographers",
                metadata = null,
                admins = emptyList(),
                members = members,
                availability = availability,
                perHost = emptyList(),
                disagreements = emptyList(),
                selected = selected,
            ),
        )

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

    /** A group write returns the ORDINARY [Receipt] -- store-issued id and
     * all (#1244) -- and its stream delivers ordinary [WriteFact]s. There is
     * no group-shaped receipt type and no untracked door left to reach. */
    @Test
    fun groupWriteFactStreamDeliversOrdinaryWriteFacts() =
        runBlocking {
            NMPEngine(NMPConfig()).use { engine ->
                val scope = NMPRelayScope.on(listOf(host(1)))
                val group = scope.group("photographers")
                val authorHex = randomPubkeyHex()

                val status = group.publish(engine, authorHex, kind = 9u, content = "hi")
                check(status.id > 0uL) {
                    "a group write must carry the store-issued receipt id like any other write"
                }
                val first = status.status.first()
                // The composed author has no registered signer, so the first
                // fact is the park itself -- acceptance is the publish call
                // returning, never a stream item.
                check(
                    first is WriteFact.Signing &&
                        first.state == SigningState.AwaitingSigner(authorHex),
                ) {
                    "expected the write to park awaiting a signer, got $first"
                }
            }
        }
}
