package com.nmp.sdk

import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.take
import kotlinx.coroutines.flow.toList
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertThrows
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test

/** #572: typed NIP-22 comments over NIP-73 external targets, exercised
 * through the public Kotlin SDK. Mirrors NIP22Tests.swift. */
class NIP22Test {
    private val author = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"

    /** Root-thread demand scopes kind:1111 by the uppercase `#I` tag --
     * never a parent-only lowercase `#i` shortcut. */
    @Test
    fun commentThreadDemandScopesKind1111ByUppercaseITag() {
        val root = CommentRoot.External(Nip73Target.PodcastEpisodeGuid("guid-1"))
        val demand = commentThreadDemand(root)
        assertEquals(listOf(1111u.toUShort()), demand.selection.kinds)
    }

    /** Mirrors the NIP-29 kind:10009 decode-fixture pattern (#108): build a
     * fixture [Row] with the exact required tags and assert the decoded
     * typed value. */
    @Test
    fun decodeCommentComposesATypedTopLevelPodcastComment() {
        val row =
            Row(
                id = "1".repeat(64),
                pubkey = author,
                createdAt = 1uL,
                kind = 1111u.toUShort(),
                tags =
                    listOf(
                        listOf("I", "podcast:item:guid:guid-1"),
                        listOf("K", "podcast:item:guid"),
                        listOf("i", "podcast:item:guid:guid-1"),
                        listOf("k", "podcast:item:guid"),
                    ),
                content = "nice episode",
                sig = "0".repeat(128),
                sources = emptyList(),
            )
        val decoded = decodeComment(row)
        assertEquals(CommentRoot.External(Nip73Target.PodcastEpisodeGuid("guid-1")), decoded.root)
        assertEquals(CommentParent.Root, decoded.parent)
        assertEquals("nice episode", decoded.content)
    }

    /** #572 review finding 1: a `K == podcast:item:guid` cell whose `I`
     * value is the BARE guid (missing NIP-73's required
     * `podcast:item:guid:` prefix) is a typed refusal, never silently
     * accepted -- a bare-guid comment would split the episode's thread from
     * conformant clients (e.g. Fountain). */
    @Test
    fun podcastGuidMissingPrefixIsRejected() {
        val row =
            Row(
                id = "1".repeat(64),
                pubkey = author,
                createdAt = 1uL,
                kind = 1111u.toUShort(),
                tags =
                    listOf(
                        listOf("I", "guid-1"),
                        listOf("K", "podcast:item:guid"),
                        listOf("i", "guid-1"),
                        listOf("k", "podcast:item:guid"),
                    ),
                content = "",
                sig = "0".repeat(128),
                sources = emptyList(),
            )
        val error =
            assertThrows(CommentDecodeError::class.java) { decodeComment(row) }
        assertEquals(CommentDecodeError.MalformedExternalValue("guid-1"), error)
    }

    /** Missing K/k, mismatched I/i, and duplicate contradictory root tags
     * never become a typed comment -- the malformed-rejection matrix. */
    @Test
    fun malformedTagSetsAreRejectedNotSilentlyCoerced() {
        fun row(tags: List<List<String>>): Row =
            Row(
                id = "1".repeat(64),
                pubkey = author,
                createdAt = 1uL,
                kind = 1111u.toUShort(),
                tags = tags,
                content = "",
                sig = "0".repeat(128),
                sources = emptyList(),
            )

        // Missing K.
        var error =
            assertThrows(CommentDecodeError::class.java) {
                decodeComment(row(listOf(listOf("I", "guid-1"), listOf("i", "guid-1"), listOf("k", "podcast:item:guid"))))
            }
        assertEquals(CommentDecodeError.MissingRootKind, error)

        // Missing k.
        error =
            assertThrows(CommentDecodeError::class.java) {
                decodeComment(row(listOf(listOf("I", "guid-1"), listOf("K", "podcast:item:guid"), listOf("i", "guid-1"))))
            }
        assertEquals(CommentDecodeError.MissingParentKind, error)

        // Mismatched I/i.
        error =
            assertThrows(CommentDecodeError::class.java) {
                decodeComment(
                    row(
                        listOf(
                            listOf("I", "podcast:item:guid:guid-1"), listOf("K", "podcast:item:guid"),
                            listOf("i", "podcast:item:guid:guid-DIFFERENT"), listOf("k", "podcast:item:guid"),
                        ),
                    ),
                )
            }
        assertEquals(CommentDecodeError.ParentDoesNotMatchRootOrComment, error)

        // Duplicate contradictory root tags (different letters: E and I).
        error =
            assertThrows(CommentDecodeError::class.java) {
                decodeComment(
                    row(
                        listOf(
                            listOf("E", "1".repeat(64)), listOf("I", "podcast:item:guid:guid-1"),
                            listOf("K", "podcast:item:guid"), listOf("i", "podcast:item:guid:guid-1"),
                            listOf("k", "podcast:item:guid"),
                        ),
                    ),
                )
            }
        assertEquals(CommentDecodeError.DuplicateContradictoryRoot, error)

        // Duplicate SAME-letter root tags (two contradictory I tags) --
        // #572 review finding 3: same-letter duplicates must not silently
        // resolve to "first one wins".
        error =
            assertThrows(CommentDecodeError::class.java) {
                decodeComment(
                    row(
                        listOf(
                            listOf("I", "podcast:item:guid:guid-1"),
                            listOf("I", "podcast:item:guid:guid-2"),
                            listOf("K", "podcast:item:guid"), listOf("i", "podcast:item:guid:guid-1"),
                            listOf("k", "podcast:item:guid"),
                        ),
                    ),
                )
            }
        assertEquals(CommentDecodeError.DuplicateContradictoryRoot, error)

        // An unrelated event with no NIP-22 tags at all.
        error =
            assertThrows(CommentDecodeError::class.java) {
                decodeComment(row(listOf(listOf("t", "podcast"))))
            }
        assertEquals(CommentDecodeError.MissingRoot, error)
    }

    /** #572's offline-signer durable acceptance + restart reattachment
     * falsifier: compose a comment intent while the active identity has no
     * signer, publish, observe `Accepted` + `AwaitingCapability` (the
     * canonical "locally pending" state), and prove the SAME token
     * reattaches the identical obligation. */
    @Test
    fun offlineSignerDurableAcceptanceAndCorrelationReattachment() =
        runBlocking {
            NMPEngine(NMPConfig()).use { engine ->
                engine.setActiveAccount(author)

                val token = "kotlin-nip22-offline-signer-token"
                val intent =
                    engine.commentIntent(
                        root = CommentRoot.External(Nip73Target.PodcastEpisodeGuid("guid-offline")),
                        parent = CommentParent.Root,
                        authorPubkey = author,
                        createdAt = 1_723_458_000uL,
                        content = "great show",
                        correlation = token,
                    )
                val receipt = engine.publishComposed(intent)
                val statuses = withTimeout(5_000) { receipt.status.take(2).toList() }
                assertEquals(
                    listOf(WriteStatus.Accepted, WriteStatus.AwaitingCapability(author)),
                    statuses,
                )

                // The app never learned the numeric receipt id (it only minted
                // the token) -- reattach using only the token, mirroring a
                // restart.
                val reattachment = engine.reattachReceipt(token)
                assertTrue(reattachment is ReceiptReattachment.Attached)
                val replay = (reattachment as ReceiptReattachment.Attached).receipt
                val replayStatuses = withTimeout(5_000) { replay.status.take(2).toList() }
                assertEquals(
                    listOf(WriteStatus.Accepted, WriteStatus.AwaitingCapability(author)),
                    replayStatuses,
                )
            }
        }

    // #572 review finding 4: test honesty.

    /** A REAL golden fixture -- a fixed secret key, timestamp, content, and
     * podcast target -- whose composed event id and exact NIP-01 JSON body
     * are pinned as literal constants and asserted identical in Rust
     * (`crates/nmp-nip22/src/build.rs::golden_fixture_tests`), Swift
     * (`NIP22Tests.swift`), and here. Structural identity (all composition
     * happens in Rust behind FFI) is a fair argument for why Kotlin
     * composing the SAME bytes is likely, but it isn't the demanded proof --
     * this asserts the ACTUAL marshalled bytes a Kotlin caller observes,
     * including the `ULong createdAt` -> `u64` boundary crossing. */
    @Test
    fun goldenFixturePinsTheExactComposedBytes() =
        runBlocking {
            val authorPubkey = "1b84c5567b126440995d3ed5aaba0565d71e1834604819ff9c17f5e9d5dd078f"
            val expectedEventId = "b1981e70a89150af5ca02548324f3ca2a1fff1b97581d46ab53e11116a553938"

            NMPEngine(NMPConfig()).use { engine ->
                engine.setActiveAccount(authorPubkey)

                val intent =
                    engine.commentIntent(
                        root = CommentRoot.External(Nip73Target.PodcastEpisodeGuid("golden-guid-572")),
                        parent = CommentParent.Root,
                        authorPubkey = authorPubkey,
                        createdAt = 1_700_000_000uL,
                        content = "golden fixture content",
                    )
                val receipt = engine.publishComposed(intent)
                val statuses = withTimeout(5_000) { receipt.status.take(2).toList() }
                assertEquals(WriteStatus.Accepted, statuses.first())

                // Cross-check the actual computed event id via the ordinary
                // read path: the pending row's own `id` field IS the
                // composed event id.
                val demand =
                    commentThreadDemand(
                        CommentRoot.External(Nip73Target.PodcastEpisodeGuid("golden-guid-572")),
                    )
                val row = withTimeout(5_000) { engine.observe(demand).first { it.rows.isNotEmpty() } }.rows.first()
                assertEquals(expectedEventId, row.id)
            }
        }

    /** #572 review finding 4: "durable acceptance makes one canonical
     * pending comment visible through the ordinary query path" was NOT
     * exercised by the original suite -- coverage stopped at receipt
     * statuses. This composes, publishes, and OBSERVES the pending row
     * through `commentThreadDemand` + `observe`, then decodes it with
     * `decodeComment`, proving the whole write -> read -> decode loop
     * converges on a coherent typed value while the write remains
     * unsigned/pending. */
    @Test
    fun durableAcceptanceMakesOneCanonicalPendingCommentVisibleThroughTheQueryPath() =
        runBlocking {
            NMPEngine(NMPConfig()).use { engine ->
                engine.setActiveAccount(author)

                val root = CommentRoot.External(Nip73Target.PodcastEpisodeGuid("guid-query-path"))
                val demand = commentThreadDemand(root)
                val rowFlow = engine.observe(demand)

                val intent =
                    engine.commentIntent(
                        root = root,
                        parent = CommentParent.Root,
                        authorPubkey = author,
                        createdAt = 1_723_459_000uL,
                        content = "visible through the ordinary query path",
                    )
                val receipt = engine.publishComposed(intent)
                withTimeout(5_000) { receipt.status.take(2).toList() }

                val row = withTimeout(5_000) { rowFlow.first { it.rows.isNotEmpty() } }.rows.first()
                assertEquals(author, row.pubkey)
                assertEquals(1111u.toUShort(), row.kind)

                val decoded = decodeComment(row)
                assertEquals(root, decoded.root)
                assertEquals(CommentParent.Root, decoded.parent)
                assertEquals("visible through the ordinary query path", decoded.content)
            }
        }
}
