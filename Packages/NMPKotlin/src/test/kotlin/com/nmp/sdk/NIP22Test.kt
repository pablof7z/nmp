package com.nmp.sdk

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
                        listOf("I", "guid-1"),
                        listOf("K", "podcast:item:guid"),
                        listOf("i", "guid-1"),
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
                            listOf("I", "guid-1"), listOf("K", "podcast:item:guid"),
                            listOf("i", "guid-DIFFERENT"), listOf("k", "podcast:item:guid"),
                        ),
                    ),
                )
            }
        assertEquals(CommentDecodeError.ParentDoesNotMatchRootOrComment, error)

        // Duplicate contradictory root tags.
        error =
            assertThrows(CommentDecodeError::class.java) {
                decodeComment(
                    row(
                        listOf(
                            listOf("E", "1".repeat(64)), listOf("I", "guid-1"),
                            listOf("K", "podcast:item:guid"), listOf("i", "guid-1"),
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
}
